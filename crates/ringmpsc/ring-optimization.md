# Ring Buffer Optimization Analysis

This document provides a detailed analysis of how `ringmpsc` is optimized for CPU cache friendliness, zero-copy data structures, and low-level performance. It covers every optimization technique used in the implementation and identifies areas for further improvement.

**Scope:** `Ring<T, A>`, `StackRing<T, N>`, `Channel<T, A>`, `StackChannel<T, N, P>`, and supporting infrastructure (`Reservation`, `Backoff`, `Metrics`, allocators).

---

## Table of Contents

1. [Architectural Optimization: Ring Decomposition](#1-architectural-optimization-ring-decomposition)
2. [CPU Cache Optimization](#2-cpu-cache-optimization)
3. [Zero-Copy Data Path](#3-zero-copy-data-path)
4. [Atomic Operation Minimization](#4-atomic-operation-minimization)
5. [Memory Ordering Optimization](#5-memory-ordering-optimization)
6. [Batch Amortization](#6-batch-amortization)
7. [Allocator-Level Optimization](#7-allocator-level-optimization)
8. [Compile-Time Optimization (StackRing)](#8-compile-time-optimization-stackring)
9. [Backoff and Contention Management](#9-backoff-and-contention-management)
10. [Branch Elimination and Inlining](#10-branch-elimination-and-inlining)
11. [Debug-Only Invariant Checking](#11-debug-only-invariant-checking)
12. [Scope for Improvement](#12-scope-for-improvement)

---

## 1. Architectural Optimization: Ring Decomposition

The most impactful optimization in `ringmpsc` is not at the instruction level — it is **structural**. Traditional MPSC queues use a single shared data structure where all producers contend on the same write cursor via CAS loops. Under high producer counts, this contention becomes the dominant bottleneck.

`ringmpsc` eliminates this entirely through **ring decomposition**: the MPSC channel is decomposed into N independent SPSC ring buffers, one per producer.

```
Producer 0  ──►  [SPSC Ring 0]  ──┐
Producer 1  ──►  [SPSC Ring 1]  ──┼──►  Consumer (polls sequentially)
Producer 2  ──►  [SPSC Ring 2]  ──┘
```

**Why this matters:**

- **Zero producer-producer contention.** Each producer writes exclusively to its own ring. No shared write cursor, no CAS loops, no cache line bouncing between producer cores. This is the reason `ringmpsc` scales near-linearly to 6+ producers while CAS-based queues degrade.

- **No CAS (Compare-And-Swap) anywhere.** Because each ring is single-producer, all writes are plain `store()` operations. CAS loops are inherently non-deterministic in latency — a producer can theoretically spin for an unbounded number of retries under contention. `ringmpsc` avoids this entirely.

- **Each ring is independently cache-friendly.** A producer's hot data (`tail`, `cached_head`, buffer slots being written) all reside in memory that no other producer ever touches. This means each producer's working set stays hot in its own core's L1/L2 cache without interference.

**Trade-off:** The consumer must poll N rings, making consumption O(N) per sweep. In practice N is small (typically 4–32), and each poll is a single Acquire load (6–20 ns), so the overhead is negligible compared to the contention elimination.

**Measured impact:** 90× faster than `crossbeam-channel`, 184× faster than `std::sync::mpsc`, peaking at 9.21 billion messages/second (6P6C).

---

## 2. CPU Cache Optimization

### 2.1 128-Byte False Sharing Prevention (INV-MEM-01)

The `Ring` struct uses `#[repr(align(128))]` wrappers (`CacheAligned<T>`) to separate hot fields by two cache lines:

```rust
#[repr(C)]
pub struct Ring<T, A: BufferAllocator = HeapAllocator> {
    // Producer hot (128-byte aligned)
    tail:        CacheAligned<AtomicU64>,
    cached_head: CacheAligned<UnsafeCell<u64>>,

    // Consumer hot (128-byte aligned)
    head:        CacheAligned<AtomicU64>,
    cached_tail: CacheAligned<UnsafeCell<u64>>,

    // Cold state (rarely accessed)
    active:      CacheAligned<AtomicBool>,
    closed:      AtomicBool,
    metrics:     Metrics,
    config:      Config,
    buffer:      UnsafeCell<A::Buffer<T>>,
}
```

**Why 128 bytes instead of 64?** Modern Intel and AMD CPUs have a **spatial prefetcher** that fetches cache lines in aligned pairs. When one core touches a 64-byte line, the spatial prefetcher speculatively pulls the adjacent 64-byte line into L2. If `tail` (producer-hot) sits in the line adjacent to `head` (consumer-hot), the prefetcher drags them into each other's caches even though they are on separate 64-byte lines. This is **prefetcher-induced false sharing** — invisible to `perf c2c` but measurable in throughput.

128-byte alignment guarantees every field starts on an **even cache-line boundary**, so the paired prefetch never pulls in another core's hot data.

**Why not `crossbeam_utils::CachePadded`?** It uses `align(64)`. That is insufficient for the spatial prefetcher issue. The custom `CacheAligned` also avoids an external dependency for a trivial 8-line struct.

### 2.2 Hot/Cold Field Separation

Fields are grouped by access frequency and access pattern:

| Zone | Fields | Accessed by | Frequency |
|------|--------|-------------|-----------|
| Producer hot | `tail`, `cached_head` | Producer only | Every `reserve()`/`commit()` |
| Consumer hot | `head`, `cached_tail` | Consumer only | Every `consume_batch()`/`advance()` |
| Cold | `active`, `closed`, `metrics`, `config` | Rare | Lifecycle events, optional metrics |

The `#[repr(C)]` attribute ensures the compiler lays out fields in declaration order, making the zone layout deterministic. A producer in its tight loop never touches the consumer's cache lines, and vice versa.

### 2.3 Sequential Buffer Access Pattern

Ring buffer traversal is inherently sequential — the producer writes slot 0, then 1, then 2, etc., wrapping at the end. This is the most favorable access pattern for hardware prefetchers:

- **Stride-1 access** triggers both L1 and L2 hardware prefetchers on Intel/AMD.
- **No software prefetch hints.** Early versions included manual `prefetch` instructions, but A/B testing showed they *hurt* performance by competing with the hardware prefetcher's more sophisticated algorithms. They were removed (see [PERFORMANCE.md](PERFORMANCE.md)).

The power-of-two capacity (INV-MEM-02) ensures the wrap-around index computation (`idx & mask`) is a single `AND` instruction (1 cycle) rather than integer division (20–90 cycles). This keeps the access pattern tight and predictable.

### 2.4 L1 Cache Sizing for Low Latency

The `LOW_LATENCY_CONFIG` preset uses `ring_bits = 12` (4,096 slots). For 8-byte elements, this is 32 KB — which fits entirely in the L1 data cache on all modern x86-64 and ARM cores (typically 32–48 KB). This means the producer and consumer operate entirely out of L1 with zero L2/L3 misses on the buffer itself.

### 2.5 NUMA-Aware Memory Placement (INV-NUMA-01)

On multi-socket systems, the `NumaAllocator` (feature `numa`) binds each ring buffer's backing memory to a specific NUMA node using `mmap` + `mbind`. Without this, the OS may place memory on a remote node, adding ~80–100 ns of cross-socket latency per cache-line miss.

Three policies are available:

| Policy | Behavior | Cache implication |
|--------|----------|-------------------|
| `Fixed(node)` | All rings on one node | Consumer-local: all reads are local |
| `RoundRobin` | Cycle across nodes | Spreads memory pressure evenly |
| `ProducerLocal` | Allocate on the calling thread's node | Writer-local: all writes are local |

Huge page support (`MAP_HUGETLB`, 2 MiB pages) is available via `NumaAllocator::with_huge_pages()` to reduce TLB misses on large buffers.

---

## 3. Zero-Copy Data Path

### 3.1 Reservation API

The `Reservation<'a, T, A>` type is the core zero-copy primitive. Instead of copying data into the ring and then copying it out, the producer writes directly into the ring buffer's memory:

```rust
// Producer writes directly into ring buffer slots — no intermediate copy
if let Some(mut reservation) = ring.reserve(batch_size) {
    let slice: &mut [MaybeUninit<T>] = reservation.as_mut_slice();
    for slot in slice.iter_mut() {
        slot.write(value);  // Write directly into ring memory
    }
    reservation.commit();   // Publish via Release store (no copy)
}
```

**What makes this zero-copy:**

1. `reserve()` returns a mutable slice pointing directly into the ring buffer's `MaybeUninit<T>` array. No temporary buffer, no staging area, no intermediate `Vec`.
2. The producer writes values in-place using `MaybeUninit::write()`.
3. `commit()` publishes the data by storing the new `tail` with `Release` ordering — a single atomic store, no memcpy.

### 3.2 Zero-Allocation Reservation Struct

The `Reservation` struct itself is allocation-free:

```rust
pub struct Reservation<'a, T, A: BufferAllocator = HeapAllocator> {
    slice: &'a mut [MaybeUninit<T>],  // Borrow into ring buffer
    ring_ptr: *const Ring<T, A>,       // Raw pointer (zero-cost)
    len: usize,                        // Cached from slice.len()
}
```

An earlier version used `Box<dyn FnOnce(usize)>` for the commit callback, which required a heap allocation and vtable dispatch on every reservation. Replacing it with a raw pointer eliminated:
- 1 heap allocation per `reserve()` call
- 1 vtable indirection per `commit()` call
- ~5–10 ns per reservation

This was the single largest optimization in the project's history: **38% improvement** on the zero-copy benchmark path.

### 3.3 Owned Consumption (Zero-Clone)

On the consumer side, `consume_batch_owned()` transfers ownership of items directly out of the buffer without cloning:

```rust
ring.consume_batch_owned(|item: T| {
    // `item` is moved out of the ring, not cloned
    collection.push(item);
});
```

Internally this uses `MaybeUninit::assume_init_read()` to move the value out of the buffer slot. For types containing heap allocations (`String`, `Vec<T>`, `HashMap`), this avoids the cost of `clone()` entirely — the heap pointers are simply transferred.

### 3.4 Contiguous Slice Guarantee

`reserve()` always returns a contiguous `&mut [MaybeUninit<T>]` slice. If the requested reservation wraps around the ring boundary, it is truncated to the contiguous portion. The caller loops:

```rust
while remaining > 0 {
    if let Some(mut r) = ring.reserve(remaining) {
        remaining -= r.len();  // May be < remaining due to wrap
        // ... write to contiguous slice ...
        r.commit();
    }
}
```

**Why contiguous-only?** A scatter-gather API (returning two slices for wrap-around) would be more flexible but adds complexity to every caller and prevents handing the slice to APIs that expect contiguous memory (e.g., `io::Write`). The contiguous guarantee also enables `memcpy`-based bulk transfers for `Copy` types.

---

## 4. Atomic Operation Minimization

### 4.1 Cached Sequence Numbers

The most critical optimization in the per-operation hot path is **cached sequence numbers**. Each side caches the other side's sequence number in a thread-local `UnsafeCell<u64>`:

```
Producer:  cached_head  (caches consumer's head)
Consumer:  cached_tail  (caches producer's tail)
```

**Fast path (no cross-core traffic):**
```rust
// Producer reserve() fast path
let tail = self.tail.load(Ordering::Relaxed);        // Local read (1 cycle)
let cached_head = unsafe { *self.cached_head.get() }; // UnsafeCell read (1 cycle)
let space = capacity - (tail - cached_head);
if space >= n { /* grant reservation */ }
```

**Slow path (only on cache miss):**
```rust
// Only when cached value says "full" — might be stale
let head = self.head.load(Ordering::Acquire);         // Cross-core read (~20 ns)
unsafe { *self.cached_head.get() = head; }             // Update cache
```

The fast path involves **zero cross-core atomic operations**: `tail.load(Relaxed)` is a local read (the producer is the sole writer of `tail`), and `cached_head` is an `UnsafeCell` with no ordering at all. The slow path fires only when the cached value indicates the ring is full — which is the rare case in a well-sized ring.

**Why `UnsafeCell` instead of `AtomicU64`?** `cached_head` is read and written by exactly one thread (the producer). Using an atomic would force unnecessary memory ordering constraints on every access. `UnsafeCell` gives raw read/write with zero overhead.

### 4.2 Single-Writer Atomics (No CAS)

Because the SPSC design guarantees exactly one writer per field:

| Field | Solo writer | Operation |
|-------|------------|-----------|
| `tail` | Producer | `store(Release)` — never `compare_exchange` |
| `head` | Consumer | `store(Release)` — never `compare_exchange` |

Plain `store()` on x86-64 is a single `mov` instruction (1 cycle). `compare_exchange` is a `lock cmpxchg` (10–25 cycles in the uncontended case, potentially hundreds under contention). The SPSC protocol eliminates CAS entirely.

### 4.3 Relaxed Self-Reads

When the producer reads its own `tail`, it uses `Ordering::Relaxed` — the cheapest possible ordering. This is correct because:

1. The producer is the sole writer of `tail`, so it always sees its own latest write.
2. No synchronization is needed with itself.

Similarly, the consumer reads its own `head` with `Relaxed`. The only `Acquire` loads occur on the slow path when reading the *other* thread's counter.

---

## 5. Memory Ordering Optimization

`ringmpsc` uses the **minimum sufficient ordering** at every atomic access point. Stronger-than-necessary ordering (e.g., `SeqCst` everywhere) would pessimize performance by inserting unnecessary memory fences.

### 5.1 Ordering Map

| Access | Ordering | Justification |
|--------|----------|---------------|
| `tail.load()` by producer | `Relaxed` | Self-read; producer always sees own write |
| `head.load()` by consumer | `Relaxed` | Self-read; consumer always sees own write |
| `head.load()` by producer | `Acquire` | Must see consumer's writes before this point |
| `tail.load()` by consumer | `Acquire` | Must see producer's writes before this point |
| `tail.store()` by producer | `Release` | Publishes buffer writes to consumer |
| `head.store()` by consumer | `Release` | Publishes consumption to producer |
| `cached_head` / `cached_tail` | None (UnsafeCell) | Single-writer, no ordering needed |
| Metrics counters | `Relaxed` | Statistical only; no control flow depends on exact values |
| `closed` flag | Acquire/Release | Lifecycle coordination, not hot path |

### 5.2 Happens-Before Chain

The correctness of the entire protocol rests on this chain:

```
producer.write(buffer[idx])
    ↓ (sequenced-before)
producer.tail.store(new_tail, Release)
    ↓ (synchronizes-with)
consumer.tail.load(Acquire)
    ↓ (sequenced-before)
consumer.read(buffer[idx])
```

The `Release` store on `tail` acts as a **memory fence** that ensures all buffer writes are visible before the consumer sees the new tail value. The `Acquire` load on `tail` ensures the consumer sees those buffer writes after loading the new tail.

### 5.3 No SeqCst

`SeqCst` (sequentially consistent) ordering is never used on the data path. The only `SeqCst` operation in the entire codebase is `producer_count.fetch_add(1, SeqCst)` in `Channel::register()` — a lifecycle operation called once per producer, not on the hot path.

`SeqCst` is expensive because it requires a **full memory barrier** (`mfence` on x86, `dmb ish` on ARM), which stalls the store buffer. The Release/Acquire pair is sufficient for the SPSC protocol and compiles to zero extra instructions on x86-64 (where all stores are already release and all loads are already acquire by the hardware memory model).

---

## 6. Batch Amortization

### 6.1 Single Head Update for N Items

`consume_batch()` processes all available items but issues only **one** atomic store of `head`:

```rust
pub fn consume_batch<F>(&self, mut handler: F) -> usize {
    let head = self.head.load(Relaxed);
    let tail = self.tail.load(Acquire);
    let avail = (tail - head) as usize;

    // Process all items — NO ATOMICS IN THIS LOOP
    let mut pos = head;
    while pos != tail {
        let item = unsafe { buffer[idx].assume_init_read() };
        handler(&item);
        pos += 1;
    }

    // Single atomic update for entire batch
    self.head.store(tail, Release);
    avail
}
```

If 1,000 items are available, this performs **2 atomic loads + 1 atomic store** total, not 3,000. The per-item amortized atomic cost approaches zero as batch size grows.

**Measured impact:** ~2 ns/item amortized for batch consumption (compared to ~6 ns per item for single-item `push()`/`pop()`).

### 6.2 Batch Reserve via Reservation

The `reserve(n)` API reserves up to `n` slots in a single call. The producer can then write all `n` items to the returned slice and `commit()` once. This amortizes the atomic store on `tail` across all items in the reservation.

### 6.3 Metrics Amortization

When metrics are enabled, counts are batched: `add_messages_sent(n)` increments by the batch count, not called once per message. Since metrics use `Relaxed` ordering, they compile to a single `lock xadd` per batch, not per item.

---

## 7. Allocator-Level Optimization

### 7.1 Zero-Sized Type (ZST) Default Allocator

`HeapAllocator` is a zero-sized type:

```rust
#[derive(Clone, Copy, Debug, Default)]
pub struct HeapAllocator;

const _: () = assert!(size_of::<HeapAllocator>() == 0);
```

This means `Ring<T>` and `Ring<T, HeapAllocator>` have **identical size and machine code**. The type parameter is purely a compile-time abstraction — it adds zero bytes to the struct layout and zero instructions to the generated code.

Cloning `HeapAllocator` (needed by `Channel::new_in()` to create per-producer rings) is free — it copies zero bytes.

### 7.2 Allocator Consumed at Construction

The allocator is used once at construction time and **never stored** in the ring:

```rust
pub fn new_in(config: Config, alloc: A) -> Self {
    let buffer = alloc.allocate::<T>(capacity);  // Used here
    Self {
        buffer: UnsafeCell::new(buffer),          // Buffer stored, allocator dropped
        // ...
    }
}
```

This avoids storing an allocator reference/pointer in every ring, saving 8 bytes per ring and one level of indirection on every buffer access. The buffer type (`A::Buffer<T>`) handles its own deallocation via `Drop`.

### 7.3 AlignedAllocator

`AlignedAllocator<ALIGN>` over-allocates by `ALIGN - 1` bytes and rounds up the interior pointer. This provides arbitrary power-of-two alignment without relying on platform-specific APIs:

```rust
// For ALIGN=128: over-allocate, then align the interior pointer
let raw = Vec::with_capacity(total_bytes);
let aligned_start = (base + ALIGN - 1) & !(ALIGN - 1);
```

Since `ALIGN` is a const generic, the alignment math is **constant-folded by the compiler** — no runtime division or branching.

### 7.4 NUMA Allocator: mmap + mbind

On Linux, `NumaAllocator` uses `mmap(MAP_ANONYMOUS | MAP_PRIVATE)` + `mbind(MPOL_BIND)` instead of the standard allocator. This gives two benefits:

1. **Page-level placement control.** `mbind` tells the kernel which NUMA node(s) should back the virtual pages, eliminating cross-socket latency.
2. **Natural huge page eligibility.** `mmap` allocations at 2 MiB boundary with `MAP_HUGETLB` use hardware huge pages directly, reducing TLB misses.

The allocator detects NUMA topology at construction time by reading `/sys/devices/system/node/` and caches the node count for the lifetime of the allocator.

---

## 8. Compile-Time Optimization (StackRing)

`StackRing<T, N>` trades runtime flexibility for maximum performance by resolving as much as possible at compile time.

### 8.1 Inline Buffer (Zero Indirection)

The buffer is embedded directly in the struct:

```rust
pub struct StackRing<T, const N: usize> {
    // ...
    buffer: [UnsafeCell<MaybeUninit<T>>; N],  // Inline, not a pointer
}
```

**Why this matters:** In `Ring<T>`, every buffer access follows a pointer: `self.buffer -> Box -> heap`. Even though the pointer is cached in L1, the indirection costs 1 data-dependent load per access and prevents the compiler from proving the buffer's address at compile time.

In `StackRing`, `buffer[idx]` compiles to `base + idx * sizeof(T)` — a single LEA instruction where `base` is a known offset from the struct pointer. The compiler can constant-fold this when the struct address is known.

**Measured impact:** 2–3× faster than heap `Ring<T>` in benchmarks (50+ billion msg/s vs. 9 billion msg/s).

### 8.2 Compile-Time Capacity Validation

`StackRing` uses a const function to validate that `N` is a power of two:

```rust
const fn assert_power_of_two<const N: usize>() {
    assert!(N > 0, "StackRing capacity must be > 0");
    assert!(N.is_power_of_two(), "StackRing capacity must be a power of 2");
}
```

This fires at compile time if the const generic is invalid, providing a zero-cost check with no runtime overhead.

### 8.3 Const Mask

The index mask is a const:

```rust
impl<T, const N: usize> StackRing<T, N> {
    const MASK: usize = N - 1;
}
```

`idx & Self::MASK` compiles to `AND reg, immediate` — the mask value is embedded directly in the instruction as a constant, requiring no register load.

### 8.4 Type-Aliased Preset Sizes

Common configurations are pre-defined as type aliases:

```rust
pub type StackRing4K<T>  = StackRing<T, 4096>;
pub type StackRing8K<T>  = StackRing<T, 8192>;
pub type StackRing16K<T> = StackRing<T, 16384>;
pub type StackRing64K<T> = StackRing<T, 65536>;
```

These enable monomorphization — the compiler generates specialized code for each capacity with the mask value baked into every instruction.

---

## 9. Backoff and Contention Management

### 9.1 Adaptive Three-Phase Backoff

The `Backoff` strategy uses an exponential progression:

| Phase | Steps | Action | Cost |
|-------|-------|--------|------|
| Spin | 0–6 | `spin_loop()` hint (PAUSE on x86) | ~5 ns/iteration |
| Yield | 7–10 | `thread::yield_now()` | ~1 µs (context switch) |
| Give up | >10 | Return `None` | Caller decides |

The spin phase uses exponential back-off: iteration count doubles each step (1, 2, 4, 8, 16, 32, 64 spins). This avoids hammering the memory bus when the ring is temporarily full.

**Why not park/wake?** Parking requires a mutex/condvar pair and a syscall to wake. For the typical case where the ring drains within microseconds, spinning is cheaper. The caller can choose to park externally if `reserve()` returns `None`.

### 9.2 `reserve_with_backoff()` Convenience

The `reserve_with_backoff()` method encapsulates the backoff loop:

```rust
pub fn reserve_with_backoff(&self, n: usize) -> Option<Reservation<'_, T, A>> {
    let mut backoff = Backoff::new();
    while !backoff.is_completed() {
        if let Some(r) = self.reserve(n) { return Some(r); }
        if self.is_closed() { return None; }
        backoff.snooze();
    }
    None
}
```

This keeps the core `reserve()` path tight and branch-free (it returns `None` immediately on full), while providing a convenient retry wrapper.

---

## 10. Branch Elimination and Inlining

### 10.1 `#[inline]` on Hot Paths

All hot-path methods are annotated with `#[inline]`:

- `capacity()`, `mask()`, `len()`, `is_empty()`, `is_full()`, `is_closed()`
- `advance()`
- `push()` (which is a thin wrapper over `reserve()` + `commit()`)

This enables the compiler to inline these into the caller's loop body. For `push()`, inlining eliminates the function call overhead and allows the compiler to merge the `reserve()` + `commit()` sequence into the surrounding code.

### 10.2 Power-of-Two Masking

Every index computation is a bitwise AND:

```rust
let idx = (sequence as usize) & mask;
```

This compiles to a single `AND` instruction on all architectures. The alternative — modulo with arbitrary capacity — would require an integer division instruction (`div` on x86, 20–90 cycles).

### 10.3 Metrics Behind Feature Flag

Metrics are controlled by `Config.enable_metrics`:

```rust
if self.config.enable_metrics {
    self.metrics.add_messages_sent(n as u64);
}
```

When `enable_metrics` is `false` (the default), the branch predictor learns to skip this after one or two iterations, making the check essentially free. The `Relaxed` ordering on metric counters ensures they do not interfere with the data path's ordering guarantees.

---

## 11. Debug-Only Invariant Checking

All invariant checks use `debug_assert!` macros that compile to **zero instructions** in release builds:

```rust
debug_assert_bounded_count!(new_tail.wrapping_sub(head) as usize, self.capacity());
debug_assert_monotonic!("tail", tail, new_tail);
debug_assert_no_wrap!("tail", tail, new_tail);
debug_assert_initialized_read!(pos, head, tail);
```

These provide comprehensive runtime verification in debug builds (catching invariant violations during development) while guaranteeing zero overhead in production.

The invariant macros reference spec IDs (INV-SEQ-01, INV-INIT-01, etc.), creating a traceable link between the formal specification and the runtime checks.

---

## 12. Scope for Improvement

### 12.1 SIMD Batch Copy for `Copy` Types

For `T: Copy` types, `reserve()` + `commit()` and `consume_batch()` could use SIMD intrinsics (`_mm256_store_si256` / `_mm256_load_si256` on AVX2) to copy 32 bytes per instruction instead of element-by-element. For 4-byte elements, this is 8× more data per instruction.

**Where it applies:** `send()`, `recv()`, and any batch path where `T` is `Copy` and small (`u8`, `u16`, `u32`, `u64`). Would require feature-gated `#[cfg(target_arch = "x86_64")]` specialization.

**Expected impact:** 2–4× improvement on bulk transfer of small `Copy` types. Diminishing returns for large `T` where memory bandwidth is the bottleneck, not instruction count.

### 12.2 Adaptive Batch Sizing

Currently, batch sizes are caller-determined. An adaptive strategy could monitor fill levels and adjust batch sizes dynamically:

- **Low fill:** Small batches (reduce latency)
- **High fill:** Large batches (maximize throughput)
- **Drain mode:** Process everything (prevent overflow)

This is essentially the feedback loop used in TCP congestion control (AIMD), adapted for ring buffers. It would be most valuable in streaming scenarios where load fluctuates (e.g., the `span_collector` pattern).

### 12.3 `io_uring`-Style Submission/Completion Rings

For scenarios where `ringmpsc` acts as an I/O submission queue (e.g., WAL writes in `ringwal`), an `io_uring`-compatible layout would allow the ring buffer's memory to be shared directly with the kernel, eliminating the syscall overhead of `read()`/`write()`. This would require:

- `mmap`-compatible buffer allocation (already available via `NumaAllocator`)
- Submission Queue Entry (SQE) and Completion Queue Entry (CQE) compatible layout
- Kernel-visible head/tail pointers

This is a significant architectural change but could yield 2–3× throughput improvement for I/O-bound workloads by eliminating the user-kernel copy.

### 12.4 Per-Core Ring Affinity

Currently, `Channel::consume_all()` polls all rings sequentially. On NUMA systems, this causes the consumer to access each ring's buffer memory, which may reside on different NUMA nodes. A per-core consumer affinity model would:

- Assign each ring to the consumer core closest to its NUMA node
- Use work stealing between consumers when load is imbalanced

This extends the current `get_ring()` API but requires coordination between consumers that `ringmpsc` currently avoids by design (single-consumer model).

### 12.5 `memcpy` Optimization for Contiguous Reservations

When the producer fills a reservation with data from a contiguous source (e.g., reading from a file or network buffer), the element-by-element `MaybeUninit::write()` loop could be replaced with a single `ptr::copy_nonoverlapping()` for `Copy` types:

```rust
// Current: element-by-element
for (slot, value) in slice.iter_mut().zip(source.iter()) {
    slot.write(*value);
}

// Potential: single memcpy
unsafe {
    ptr::copy_nonoverlapping(
        source.as_ptr(),
        slice.as_mut_ptr().cast(),
        slice.len(),
    );
}
```

This is safe because `MaybeUninit<T>` has the same layout as `T`, and the reservation guarantees exclusive access to the slice. A `Reservation::copy_from_slice()` convenience method would encapsulate this.

### 12.6 Lock-Free Metrics via Thread-Local Aggregation

Current metrics use `fetch_add(n, Relaxed)` which, while cheap, still requires a `lock xadd` on x86-64 (~10 ns). For extremely hot paths, thread-local counters flushed periodically would eliminate all atomic overhead from metrics:

```rust
thread_local! {
    static LOCAL_SENT: Cell<u64> = Cell::new(0);
}
// Hot path: LOCAL_SENT.set(LOCAL_SENT.get() + n);  // 1 cycle
// Periodic: global.fetch_add(LOCAL_SENT.take(), Relaxed);
```

This trades exact real-time accuracy for zero atomic overhead on the hot path. Since metrics are already `Relaxed` (eventual consistency), the semantic change is minimal.

### 12.7 `consume_batch` with Iterator API

Currently, `consume_batch()` takes a closure `FnMut(&T)`. An iterator-based API would integrate with Rust's iterator ecosystem:

```rust
// Potential
for item in ring.drain() {
    process(item);
}

// Or with combinators
let sum: u64 = ring.drain().map(|x| x.value).sum();
```

This requires careful design to ensure the single head update is still deferred to the iterator's `Drop` (similar to `Vec::drain()`). The challenge is maintaining the batch amortization guarantee while providing ergonomic iteration.

### 12.8 Compile-Time Dispatch for StackRing Reserve

`StackRing::reserve()` currently checks `n > N` at runtime. Since `N` is a const generic, certain call sites where `n` is also a constant could benefit from compile-time elimination of this check. A `reserve_const<const M: usize>()` variant could statically assert `M <= N`:

```rust
pub unsafe fn reserve_const<const M: usize>(&self) -> Option<(*mut T, usize)>
where
    Assert<{ M <= N }>: IsTrue,
{
    // n > N check eliminated at compile time
}
```

This requires Rust's `generic_const_exprs` feature (currently nightly-only) but would eliminate a branch from the hottest path.

---

## Summary

The optimization strategy in `ringmpsc` follows a clear hierarchy:

1. **Architecture first** — Ring decomposition eliminates the #1 bottleneck (producer contention) at the design level.
2. **Cache layout second** — 128-byte alignment, hot/cold separation, and NUMA placement minimize cache misses.
3. **Zero-copy third** — Reservation API eliminates data movement; owned consumption eliminates clones.
4. **Atomics last** — Cached sequence numbers, minimum ordering, and batch amortization reduce the remaining overhead to near-zero.

Each layer builds on the previous: zero-copy only helps if cache misses are already minimized, and cache optimization only helps if contention is already eliminated. The result is a system that achieves 9+ billion messages/second on commodity hardware with a safe Rust API.
