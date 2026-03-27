# RingMPSC — Frequently Asked Questions

## 1. Why does ringmpsc use 128-byte alignment instead of 64-byte?

A single cache line on x86-64 is 64 bytes, but **128-byte alignment** is required because of the **spatial prefetcher** on modern Intel and AMD CPUs.

The spatial prefetcher automatically fetches cache lines in **aligned pairs** — when one core touches a 64-byte line, the hardware may speculatively pull the adjacent 64-byte line into L2 as well. If `head` (consumer-hot) sits in the line adjacent to `tail` (producer-hot), the prefetcher drags them into each other's caches even though they are on separate 64-byte lines. This causes **prefetcher-induced false sharing**, degrading performance on the hot path.

By aligning each hot field to 128 bytes (two cache lines), we guarantee that every field starts on an **even cache-line boundary**. The paired prefetch never pulls in another core's hot data.

This is why the `Ring` struct uses a custom `CacheAligned<T>` wrapper with `#[repr(align(128))]` instead of `crossbeam_utils::CachePadded` (which uses only 64-byte alignment). The custom wrapper also avoids an external dependency for what is a trivial 8-line type.

The struct layout groups fields into 128-byte-aligned zones:

| Zone | Fields | Accessed by |
|------|--------|-------------|
| Producer hot | `tail`, `cached_head` | Producer only (or producer-read by consumer) |
| Consumer hot | `head`, `cached_tail` | Consumer only (or consumer-read by producer) |
| Cold | `active`, `closed`, `metrics` | Rarely touched — no contention concern |

> **Spec reference:** INV-MEM-01 in [spec.md](spec.md)

---

## 2. Is the ring implementation lock-free? Is it wait-free?

**Lock-free: yes.** Every operation completes in a bounded number of steps per invocation and never acquires a lock.

- `reserve()` does at most two atomic loads (Relaxed on `tail`, Acquire on `head` for the slow path) and returns `Some` or `None`. There is no CAS loop.
- `commit_internal()` performs one Relaxed load and one Release store. O(1).
- `consume_batch()` / `readable()` follow the same pattern: Relaxed load of `head`, Acquire load of `tail` on cache miss, a linear scan of available items, and a single Release store of `head`. O(n) in batch size with no retries.

Because this is an SPSC ring, each atomic is written by exactly one thread. All writes are plain `store()`, never `compare_exchange()`, so there is no contention-driven retry loop. No thread can be indefinitely delayed by the actions of the other.

**Wait-free: no.** Wait-freedom requires that every call completes useful work in a bounded number of the caller's own steps, regardless of what the other thread does. The ring violates this:

- `reserve()` returns `None` when the buffer is full — the producer cannot make progress until the consumer advances `head`. A stalled consumer starves the producer indefinitely.
- `consume_batch()` returns 0 when the buffer is empty — the consumer cannot make progress until the producer advances `tail`. A stalled producer starves the consumer.

Both sides can be blocked indefinitely waiting for the other, which is the classic bounded-buffer trade-off. True wait-freedom in a bounded buffer would require either unbounded memory or helping mechanisms that add complexity and overhead inappropriate for a high-performance SPSC ring.

| Property | Status | Reason |
|----------|--------|--------|
| Lock-free | **Yes** | No locks, no CAS loops, single-writer atomics only |
| Wait-free | **No** | Producer blocks on full ring, consumer blocks on empty ring |

---

## 3. Why ring decomposition instead of a traditional MPSC queue?

Traditional MPSC queues (e.g., `crossbeam-channel`, `tokio::sync::mpsc`) use a single shared data structure where **all producers contend** on the same write cursor via CAS loops. Under high producer counts, this becomes the dominant bottleneck.

RingMPSC uses **ring decomposition**: each producer gets its own dedicated SPSC ring buffer. The consumer polls all rings sequentially on a single thread. This has several advantages:

- **Zero producer-producer contention** — producers never touch each other's data.
- **No CAS loops** — each SPSC ring uses single-writer stores, not compare-and-swap.
- **Cache-friendly** — each producer's hot data lives on its own cache lines.
- **Predictable latency** — no CAS retries means bounded per-operation cost.

The trade-off is that the consumer must poll N rings, making consumption O(N) per sweep. In practice, N is small (number of producers) and the per-ring poll is extremely cheap (one Acquire load).

---

## 4. Why unbounded u64 sequence numbers instead of wrapped indices?

`head` and `tail` are monotonically increasing `u64` values. The buffer index is computed only when accessing a slot: `idx = sequence & mask`.

This design prevents the **ABA problem** entirely. With wrapped indices, a sequence number could cycle back to a previously seen value, making it impossible to distinguish old from new. With 2^64 possible values, wrap-around at 10 billion messages/second takes ~58 years — effectively impossible.

This also simplifies the protocol: the number of items in the ring is always `tail - head`, monotonicity is trivially checkable, and there is no ambiguity between "full" and "empty" states (a classic pitfall of wrapped-index ring buffers).

> **Spec reference:** INV-SEQ-03 in [spec.md](spec.md)

---

## 5. Why does `reserve(n)` sometimes return fewer than n slots?

`reserve(n)` returns a contiguous slice of `MaybeUninit<T>` slots. Because the buffer is circular, the available space may **wrap around** the end of the underlying array. Rather than returning a discontiguous pair of slices, `reserve()` returns only the contiguous portion up to the buffer boundary.

This means `reservation.len()` may be less than the requested `n`. Callers must loop:

```rust
while remaining > 0 {
    if let Some(mut r) = ring.reserve(remaining) {
        remaining -= r.len(); // may be < remaining
        r.as_mut_slice()[0].write(item);
        r.commit();
    }
}
```

This keeps the API simple and zero-copy — no internal buffering, no double writes, no scatter-gather complexity.

If you need all-or-nothing semantics, `available_count()` provides a hint. However, in a concurrent (SPSC) setting, `available_count()` is **advisory**: the consumer may advance `head` between the call to `available_count()` and the call to `reserve()`, meaning the count can only grow, never shrink, from the producer's perspective. Conversely, no other producer can steal slots (there is only one producer per ring), so an available-count check is safe as a lower bound. A check guarantees "at least this many slots are free right now" but not "exactly this many remain free by the time `reserve()` runs" in general multi-producer contexts.

> **Spec reference:** INV-RES-01 in [spec.md](spec.md)

---

## 6. Why use `UnsafeCell` for cached sequence numbers instead of atomics?

`cached_head` and `cached_tail` are each accessed by **exactly one thread**:

- `cached_head` is read and written only by the producer.
- `cached_tail` is read and written only by the consumer.

Using atomics for these fields would be wasteful — the compiler and CPU would enforce unnecessary ordering constraints on every access. `UnsafeCell<u64>` gives direct, zero-overhead reads and writes with no atomic fences.

These fields exist to **avoid cross-core traffic**: instead of loading the other thread's atomic on every operation, each side caches the last-known value and only refreshes (via an Acquire load) when the cache indicates insufficient space or data.

> **Spec reference:** INV-SW-01 and INV-SW-02 in [spec.md](spec.md)

---

## 7. Why a custom `CacheAligned<T>` instead of `crossbeam_utils::CachePadded`?

Two reasons:

1. **Alignment value** — `CachePadded` uses `#[repr(align(64))]`. RingMPSC needs `#[repr(align(128))]` for prefetcher false-sharing prevention (see FAQ #1).
2. **No external dependency** — `CacheAligned` is a trivial struct with `new()`, `Deref`, and a `repr` attribute. Pulling in `crossbeam-utils` for this adds a dependency with no benefit.

---

## 8. Why must the buffer capacity be a power of two?

Index wrapping is the hottest operation in the ring — it runs on every `reserve()`, every `consume_batch()`, every slot access. With a power-of-two capacity, wrapping is a single bitwise AND (`idx & mask`) instead of a modulo operation (`idx % capacity`). On x86-64, AND is 1 cycle; integer division is 20–90 cycles.

The `Config` constructor enforces this at creation time, and `StackRing` enforces it at compile time.

> **Spec reference:** INV-MEM-02 in [spec.md](spec.md)

---

## 9. What does NUMA-aware allocation give me and when should I use it?

On multi-socket servers, memory accesses to a remote NUMA node cost 1.5–2× the latency of local accesses. By default, `HeapAllocator` lets the OS place ring buffer memory on whatever node happens to be convenient. `NumaAllocator` (behind the `numa` feature flag) uses `mmap` + `mbind` on Linux to **pin each ring buffer's backing memory to a specific NUMA node**, keeping producer-hot data local to the core that writes it.

Use it when:

- You run on **multi-socket** hardware (2+ NUMA nodes) and care about tail latency.
- Producer threads are pinned to specific cores/nodes and you want their ring buffers co-located.
- You see **cross-node memory traffic** in `perf stat` (`node-load-misses`, `node-store-misses`).

Skip it when:

- Your system is single-socket (UMA) — `NumaAllocator` detects this and falls back to `HeapAllocator` automatically.
- Producers are not pinned — thread migration defeats the benefit of NUMA binding.

Three policies are available:

| Policy | Behavior | Best for |
|--------|----------|----------|
| `Fixed(node)` | All rings on one specified node | Consumer-local placement |
| `RoundRobin` | Cycle across all detected nodes | Spreading memory pressure evenly |
| `ProducerLocal` | Allocate on the calling thread's node | Per-producer placement when constructing rings from producer threads |

> **Platform note**: NUMA binding via `libc::mbind` is Linux-only. On macOS and Windows, `NumaAllocator` transparently falls back to heap allocation. See FAQ #10 and CUSTOM_ALLOCATORS.md for details.

> **Spec reference:** INV-NUMA-01, INV-NUMA-02, INV-NUMA-03 in [spec.md](spec.md)

---

## 10. Does `NumaAllocator` work on macOS / Windows / non-Linux?

Yes, it compiles and runs everywhere. On non-Linux platforms, `NumaAllocator` **transparently falls back** to heap allocation (`Box<[MaybeUninit<T>]>`). No conditional compilation is needed in user code — the same `Channel::<T, NumaAllocator>::new_in(...)` call works on any platform.

This is guaranteed by **INV-NUMA-02** (graceful fallback): on unsupported platforms, the allocator reports `num_nodes() == 1` and `is_numa_available() == false`, and every allocation goes through the standard allocator.

---

## 11. Why does `ProducerLocal` not always do what I expect with `Channel`?

`Channel::new_in()` pre-allocates **all** ring buffers at construction time, on a single thread. If you pass `NumaPolicy::ProducerLocal`, every ring resolves to the *channel creator's* NUMA node — not the producer's node.

For true per-producer NUMA placement, construct individual `Ring::new_in()` instances from each producer thread:

```rust
// On producer thread (pinned to NUMA node 1):
let ring = Ring::<u64, NumaAllocator>::new_in(
    Config::new(1024),
    NumaAllocator::new(NumaPolicy::ProducerLocal),
);
// Backing memory is now on node 1.
```

This is a conscious design trade-off: `Channel` values simplicity and a single-allocation constructor over thread-aware allocation. When NUMA topology matters, the lower-level `Ring` API gives full control.

---

## 12. What are huge pages and when should I enable them?

`NumaAllocator::with_huge_pages(policy)` adds `MAP_HUGETLB` to the `mmap` flags on Linux, requesting **2 MiB pages** instead of the default 4 KiB pages. This reduces TLB (Translation Lookaside Buffer) misses when traversing large ring buffers.

Enable huge pages when:

- Ring buffers are large (≥ 64 KiB, ideally ≥ 1 MiB).
- The system has huge pages reserved (`/proc/sys/vm/nr_hugepages > 0`).
- `perf stat` shows significant `dTLB-load-misses` or `dTLB-store-misses`.

If huge pages are not available, the `mmap` call fails and the allocator falls back to regular pages automatically. There is no crash or error — just standard 4 KiB pages.
