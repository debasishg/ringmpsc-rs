# Ring Buffer Specification

This document defines the invariants that ALL ring buffer implementations (`Ring<T>`, `StackRing<T, N>`) must satisfy. Violations indicate bugs.

## 1. Memory Layout Invariants

### INV-MEM-01: Cache Line Alignment
Hot atomic fields (`tail`, `head`, `cached_head`, `cached_tail`) must be 128-byte aligned to prevent false sharing on Intel/AMD CPUs that prefetch adjacent cache lines.

**Implementation**: `CacheAligned<T>` wrapper with `#[repr(align(128))]`
**Location**: [src/ring.rs#L56-L68](../src/ring.rs#L56-L68)

### INV-MEM-02: Power-of-Two Capacity
Buffer capacity must be a power of 2 to enable efficient index wrapping via bitwise AND (`idx & mask`) instead of modulo.

**Enforced by**: `Config::new()` panics if `ring_bits` invalid; `StackRing` uses compile-time assertion
**Test**: Config validation tests

### INV-MEM-03: Fixed-Size Buffer
Buffer size is determined at construction and never changes. No resizing, no reallocation.

**Implementation**: `Box<[MaybeUninit<T>]>` (heap) or `[MaybeUninit<T>; N]` (stack)

### INV-MEM-04: Allocator Safety Contract
Custom allocators (via `BufferAllocator`) must satisfy:
1. `allocate(capacity)` returns a buffer of exactly `capacity` elements.
2. The memory is valid for reads and writes for the buffer's lifetime.
3. The buffer's `Deref`/`DerefMut` targets are contiguous slices.
4. The buffer's `Drop` correctly deallocates the memory.

Violating these invariants causes undefined behavior in the ring buffer's unsafe hot path (depends on INV-INIT-01).

**Enforced by**: `unsafe trait BufferAllocator` — the `unsafe` keyword places the burden of proof on the implementor.
**Location**: [src/allocator.rs](../src/allocator.rs)

### INV-ALLOC-01: Alignment Guarantee
`AlignedAllocator<ALIGN>` must produce buffer pointers aligned to `ALIGN` bytes. `ALIGN` must be a power of two and ≥ `align_of::<T>()`.

**Implementation**: Over-allocates by `ALIGN - 1` bytes, rounds up the interior pointer.
**Verified by**: `debug_assert!` in `AlignedAllocator::allocate()`, runtime test `test_aligned_allocator_buffer_is_actually_aligned`.

### INV-ALLOC-02: Zero Overhead Default
`HeapAllocator` is a zero-sized type. `Ring<T>` and `Ring<T, HeapAllocator>` have identical size and machine code. No indirection or vtable.

**Verified by**: `static_assert!(size_of::<HeapAllocator>() == 0)` (compile-time)

### INV-NUMA-01: Memory Placement
`NumaAllocator::allocate()` must place memory on the requested NUMA node (when `mbind` succeeds). On single-node systems or when `mbind` returns `ENOSYS`/`EINVAL`, pages remain on the default node.

**Verified by**: `/proc/self/numa_maps` inspection in tests (Linux only), `debug_assert_numa_placement!` macro.

### INV-NUMA-02: Fallback Safety
On non-NUMA platforms (macOS, Windows), `NumaAllocator` must produce a valid buffer satisfying INV-MEM-04. The fallback delegates to `HeapAllocator`.

**Verified by**: `test_numa_allocator_fallback` in `tests/numa_tests.rs`.

### INV-NUMA-03: Policy Determinism
`NumaPolicy::RoundRobin` assigns nodes in strictly increasing cyclic order across sequential `allocate()` calls: `0, 1, ..., N-1, 0, 1, ...` where N is the number of NUMA nodes.

**Verified by**: `test_numa_allocator_round_robin` in `tests/numa_tests.rs`, `debug_assert!` on counter monotonicity.

## 2. Sequence Number Invariants

### INV-SEQ-01: Bounded Count
```
0 ≤ (tail - head) ≤ capacity
```
The number of items in the ring never exceeds capacity and is never negative.

**Verified by**: `len()`, `is_full()`, `is_empty()` methods

### INV-SEQ-02: Monotonic Progress
```
head_new ≥ head_old
tail_new ≥ tail_old
```
Head and tail only increase (using wrapping arithmetic). They never decrease.

**Enforced by**: Only `commit_internal()` advances tail, only `advance()` advances head

### INV-SEQ-03: ABA Prevention via Unbounded Sequences
Using u64 sequences instead of wrapped indices prevents ABA problem. At 10 billion msg/sec, wrap-around takes ~58 years.

**Critical for**: Lock-free correctness without epoch-based reclamation

### INV-SEQ-04: Producer Cache Bound
```
0 ≤ (tail - cached_head) ≤ capacity
```
The producer's cached copy of `head` lags the real `head`, but `tail` never runs more than `capacity` past it. It holds because the producer only reserves within `free_space(tail, cached_head)`, so it never commits past `cached_head + capacity`; and each refresh sets `cached_head` to the real `head`, where INV-SEQ-01 gives the same bound.

**Verified by**: `debug_assert_cached_head_bound!` in the `reserve` fast path of `Ring` and `StackRing`. Being a `debug_assert!`, it runs only in debug builds: the unit tests and Miri, not the `--release` stress tests. It is the `hstale` premise of the Lean theorem `free_space_stale_conservative`, which shows that a stale `cached_head` never reports more free space than the real `head`.

## 3. Memory Initialization Invariants

### INV-INIT-01: Initialized Range
```
buffer[i] is initialized  ⟺  head ≤ sequence(i) < tail
```
Slots in range `[head, tail)` contain valid `T` values written by the producer.

**Enforced by**: only a commit moves `tail`, so every commit must publish written slots only.
- `Ring`: commits go through `Reservation`, which counts the slots written into it (`push`, `extend`, `extend_from_slice`) and publishes only that prefix. There is no safe access to the raw slots, so an unwritten slot can't be published (#32).
- `StackRing` / `StackProducer`: `commit` is an `unsafe fn` whose contract requires the first `n` reserved slots to have been written (#15). It can't be checked, so it can't be safe.

### INV-INIT-02: Uninitialized Range  
```
buffer[i] is uninitialized  ⟺  sequence(i) < head ∨ sequence(i) ≥ tail
```
Slots outside `[head, tail)` are logically empty. `reserve()` returns `&mut [MaybeUninit<T>]` because these slots have no valid data yet.

### INV-INIT-03: Reservation Exclusivity
```
reservation.slice ⊆ buffer[tail..tail+n]  (before commit)
```
A `Reservation` grants exclusive write access to uninitialized slots. The producer must write valid `T` values before calling `commit()`.

**Location**: [src/reservation.rs](../src/reservation.rs)

## 4. Single-Writer Invariants (SPSC Property)

### INV-SW-01: Producer-Owned Fields
| Field | Writer | Reader |
|-------|--------|--------|
| `tail` | Producer only | Consumer (Acquire) |
| `cached_head` | Producer only | Producer only |

### INV-SW-02: Consumer-Owned Fields
| Field | Writer | Reader |
|-------|--------|--------|
| `head` | Consumer only | Producer (Acquire) |
| `cached_tail` | Consumer only | Consumer only |

**Enforced by**: every consumer method on `Ring` takes `&mut self`, and a `Channel` (or `StackChannel`) hands out at most one `Consumer` (`StackConsumer`) at a time, whose methods also take `&mut self`. See INV-CH-02. `StackRing`'s consumer methods take `&self` (it is `Sync`, and `StackChannel::get_ring` hands out `&StackRing`), so each is an `unsafe fn` (`peek`, `advance`, `consume_batch`, `consume_batch_owned`, `consume_up_to`) whose contract requires every consumer call on the ring to be ordered by happens-before with every other: one thread, or a Release/Acquire hand-off such as `StackChannel::consumer`. Calls that merely do not overlap in time are not enough, since a stale `head` load could then move `head` backwards.

### INV-SW-03: Buffer Slot Ownership
```
buffer[idx] written by producer  →  head ≤ idx < tail
buffer[idx] read by consumer     →  head ≤ idx < tail
```
Producer and consumer never access the same slot simultaneously because:
- Producer writes to `[tail, tail+n)` then publishes via Release on tail
- Consumer reads from `[head, tail)` after Acquire on tail

## 5. Memory Ordering Invariants

### INV-ORD-01: Producer Publish Protocol
```rust
// Fast path (cached)
tail.load(Relaxed)           // Only producer writes tail
cached_head (UnsafeCell)     // No ordering needed - single writer

// Slow path (refresh cache)
head.load(Acquire)           // Synchronizes with consumer's Release

// Commit
write_data_to_buffer()       // No ordering - protected by protocol
tail.store(new_tail, Release) // PUBLISHES writes to consumer
```

### INV-ORD-02: Consumer Read Protocol
```rust
// Fast path (cached), taken only if cached_tail > head
head.load(Relaxed)           // Only consumer writes head
cached_tail (UnsafeCell)     // No ordering needed - single writer

// Slow path (refresh cache), whenever cached_tail <= head
tail.load(Acquire)           // SYNCHRONIZES with producer's Release

// Advance
read_data_from_buffer()      // No ordering - protected by protocol
head.store(new_head, Release) // Publishes consumption to producer
```

`cached_tail` may trail `head`: `consume_batch*` and `consume_up_to*` advance `head` from a direct Acquire load of `tail`, and `advance` moves it without loading `tail` at all. Neither updates the cache. So the fast path must *compare* `cached_tail > head`, not test `cached_tail.wrapping_sub(head) != 0`, which wraps when the cache trails (#17). The same holds for `StackRing::peek`.

### INV-ORD-03: Happens-Before Chain
```
producer.write(data) → producer.tail.store(Release)
    ↓ (synchronizes-with)
consumer.tail.load(Acquire) → consumer.read(data)
```

Head never passes tail: `advance(n)` requires `n ≤ tail - head`.

**Enforced by**: `assert_head_not_past_tail!` in `Ring::advance` and `StackRing::advance`. Unlike the other invariant macros, it is on in every build profile, because a head past tail makes the next consume read unwritten slots (#14). It compares `n` with the available count, so an `n` that would wrap `head + n` around is rejected too.

## 6. Reservation Invariants

### INV-RES-01: Partial Reservation
`reserve(n)` may return a `Reservation` with `len() < n` due to buffer wrap-around. The ring provides contiguous slices only.

**Critical Pattern**:
```rust
while remaining > 0 {
    if let Some(mut r) = ring.reserve(remaining) {
        remaining -= r.len(); // MAY BE < remaining!
        // ... write ...
        r.commit();
    }
}
```

### INV-RES-02: Commit-or-Drop
A `Reservation` must either:
1. Call `commit()` to publish writes, OR
2. Be dropped without commit (writes discarded, tail unchanged)

### INV-RES-03: Pointer Validity
The raw `ring_ptr` in `Reservation` is valid for lifetime `'a` because:
1. The slice borrows from Ring's buffer with `'a`
2. Producer holds `Arc<Ring<T>>`, ensuring Ring outlives Reservation

**Location**: [src/reservation.rs#L38-L62](../src/reservation.rs#L38-L62)

## 7. Drop Safety Invariants

### INV-DROP-01: Ring Cleanup
`Ring::drop()` must drop all items in `[head, tail)` to prevent memory leaks for types that own heap allocations.

**Implementation**: [src/ring.rs#L680-L695](../src/ring.rs#L680-L695)
```rust
fn drop(&mut self) {
    for i in 0..count {
        let idx = ((head as usize).wrapping_add(i)) & mask;
        unsafe { ptr::drop_in_place(buffer[idx].as_mut_ptr()); }
    }
}
```

### INV-DROP-02: Consumption Cleanup
`consume_batch()` transfers ownership via `assume_init_read()`. Items are dropped after the handler returns.

### INV-DROP-03: No Double-Drop
Each item is dropped exactly once:
- Either by `Ring::drop()` (unconsumed items)
- Or by consumption (handler receives ownership)

## 8. Channel-Level Invariants

### INV-CH-01: One Ring Per Producer
Each `Producer<T>` is assigned a unique `Ring<T>`. No two producers share a ring.

**Enforced by**: `register()` hands out each ring id once. `Producer` and `StackProducer` are not `Clone`, and they are `Send` but not `Sync`, so their `&self` methods (`push`, `send`) can't run on two threads at once (INV-SW-01). Safe code has no other way to write a ring: `Channel::get_ring` hands out `&Ring`, which can't reserve. `StackChannel::get_ring` hands out `&StackRing`, whose `reserve` / `commit` are `unsafe fn`s. Their contract (the producer contract in `StackRing`'s docs) requires every producer call on a ring to be ordered by happens-before with every other, and rules out calls made around a live `StackProducer` for that ring. Calls that merely don't overlap in time are not enough.

### INV-CH-02: Single Consumer
At most one consumer drains a channel's rings at any time. No concurrent consumption of the same ring.

A consume moves items out of `[head, tail)` with `assume_init_read()` before it publishes the new head, so two concurrent consumers would both move out the same items, violating INV-DROP-03.

**Enforced by**:
- `Channel::consumer()` claims a flag with `swap(true, Acquire)` and fails with `ChannelError::ConsumerTaken` while a `Consumer` exists. `Consumer::drop` releases it with `store(false, Release)`, so a later consumer, on any thread, happens-after everything the previous one did.
- `Consumer` is not `Clone`, and its methods take `&mut self`.
- `Channel::get_ring()` hands out `&Ring`, which cannot consume (consumer methods need `&mut Ring`).
- `StackChannel::consumer()` / `StackConsumer` follow the same protocol.

**Verified by**: `tests/miri_double_consumer_test.rs` (Miri, both borrow models), `compile_fail` doctests on `Consumer`, `StackConsumer` and `Channel::get_ring`.

### INV-CH-03: Per-Producer FIFO
Messages from a single producer are received in send order. No global ordering across producers.

### INV-CH-04: Registration Bound and Close
The registered producer count never exceeds `max_producers` (`P` for `StackChannel`), not even for an instant, so `rings[..count]` is always in bounds. Every `register()` that succeeds is ordered before `close()`, which then closes that producer's ring; every `register()` ordered after `close()` fails with `Closed`. A closed ring accepts no new writes: a `reserve` that starts after `close()` returns gets `None`, and `push` / `send` return `false` / `0`, on both `Ring` and `StackRing`. One already in progress may still commit, because `close()` doesn't wait for producers, so a final drain belongs after the producers are done. The channel's `is_closed()` turns true only after every ring is closed, so during a close `register()` can already fail with `Closed` while `is_closed()` is still false. Items committed before the close can still be consumed. `producer_count()` synchronizes with `register()`: seeing `k` means seeing what the first `k` registering threads did before registering.

**Enforced by**: the count and a closed bit share one atomic word (`src/registry.rs`). `register()` claims an id with a CAS loop that fails instead of passing the limit, and `close()` sets the bit with one `fetch_or`, which returns the count at that moment, closes those rings, and only then sets the channel flag that `is_closed()` reads (Release / Acquire). All the claims and the `fetch_or` are read-modify-writes on the same word, so they fall in one total order without `SeqCst`. A successful claim is AcqRel and `producer_count()` loads with Acquire. `Ring::reserve_shared` and `StackRing::reserve` check the ring's closed flag.

### INV-CH-05: Lossless Close (send gates)
For code that sends through producers and reports success to its callers: every send that got a section from `SendGates::begin` commits before the consumer's final drain, so that drain sees its item. A send whose `begin` comes after `SendGates::close` gets `None` and sends nothing.

**Enforced by**: `src/gate.rs`. One word per producer id, each on its own cache line, so a send touches only its own producer's line. `begin` is `compare_exchange(0, IN_SECTION)` (Relaxed on success, which reads only the initial 0 or the producer's own earlier end; Acquire on failure, pairing with `close`), the section's end is `fetch_and(!IN_SECTION)` (Release), and `close` is `fetch_or(CLOSED)` (Release) on every gate. These are read-modify-writes on one word per gate, so for each gate the open and the close are ordered: an open after the close fails, and an open before it shows `IN_SECTION` until its end. The consumer's final drain waits for `is_quiet()` (every gate exactly `CLOSED`, Acquire), which reads from a release sequence headed by each section's end, so it sees every commit. Sections are one non-blocking reserve and commit, never a wait for space.

---

## Verification

| Invariant | Test Coverage | debug_assert! Location |
|-----------|--------------|------------------------|
| INV-MEM-01 | Manual inspection (no runtime check possible) | N/A (structural) |
| INV-MEM-02 | Compile-time assertions | `config.rs`, `stack_ring.rs` |
| INV-MEM-03 | Structural (no resize API) | N/A (structural) |
| INV-SEQ-01 | [tests/integration_tests.rs](tests/integration_tests.rs) | `invariants.rs` → `ring.rs`, `stack_ring.rs` |
| INV-SEQ-02 | [tests/integration_tests.rs](tests/integration_tests.rs) | `invariants.rs` → `ring.rs`, `stack_ring.rs` |
| INV-SEQ-03 | [tests/integration_tests.rs](tests/integration_tests.rs) | `invariants.rs` → `ring.rs`, `stack_ring.rs` |
| INV-SEQ-04 | Lean `free_space_stale_conservative` (premise `hstale`, assumed there); dynamically only in debug builds: the unit tests and Miri runs (release stress tests compile the `debug_assert!` out) | `invariants.rs` → `ring.rs`, `stack_ring.rs` (`reserve` fast path) |
| INV-INIT-01 | [tests/miri_tests.rs](tests/miri_tests.rs) (UB detection) | `invariants.rs` → `ring.rs`, `stack_ring.rs` |
| INV-INIT-02 | [tests/miri_tests.rs](tests/miri_tests.rs) (UB detection) | N/A (reservation API prevents) |
| INV-INIT-03 | Structural (borrow checker) | N/A (structural) |
| INV-SW-* | [tests/loom_tests.rs](tests/loom_tests.rs) (exhaustive interleavings) | N/A (verified by Loom) |
| INV-ORD-* | [tests/loom_tests.rs](tests/loom_tests.rs) | N/A (verified by Loom) |
| INV-RES-01 | API design, tested | N/A (API design) |
| INV-RES-02 | Structural (Drop impl) | N/A (structural) |
| INV-RES-03 | [tests/miri_tests.rs](tests/miri_tests.rs) | `invariants.rs` → `reservation.rs` |
| INV-DROP-01 | [tests/miri_tests.rs](tests/miri_tests.rs) + manual review | `invariants.rs` → `ring.rs`, `stack_ring.rs` `Drop` impls |
| INV-DROP-02 | [tests/miri_tests.rs](tests/miri_tests.rs) | N/A (verified by Miri — `assume_init_read` + RAII) |
| INV-DROP-03 | `DropTracker` unit tests in `ring.rs`, `stack_ring.rs` | N/A (tested) |
| INV-CH-01 | Config validation | `config.rs` assertions |
| INV-CH-02 | Structural (single consumer API) | N/A (structural) |
| INV-CH-03 | [tests/integration_tests.rs](tests/integration_tests.rs), [tests/miri_stack_overlap_test.rs](tests/miri_stack_overlap_test.rs) (concurrent, under Miri) | `invariants.rs` → `channel.rs`, `stack_channel.rs` |
| INV-CH-05 | [tests/loom_tests.rs](tests/loom_tests.rs) (`loom_gate_section_before_close_is_drained`, on the real gates), `gate.rs` unit tests | `invariants.rs` → `gate.rs` |
| INV-CH-04 | [tests/loom_tests.rs](tests/loom_tests.rs) (`loom_register_vs_close`, `loom_register_never_exceeds_max`, on the real `Registry`), [tests/close_register_tests.rs](tests/close_register_tests.rs) | `invariants.rs` → `registry.rs` |
| INV-MEM-04 | `unsafe trait` contract, [tla/RingSPSC.qnt](tla/RingSPSC.qnt) (`allocatorCapacityCorrect`) | N/A (proof obligation on implementor) |
| INV-NUMA-02 | Non-Linux fallback path | `invariants.rs` → `numa.rs` non-Linux `allocate()` |
| INV-ALLOC-01 | [tests/allocator_tests.rs](tests/allocator_tests.rs), [tla/RingSPSC.qnt](tla/RingSPSC.qnt) (`alignmentGuarantee`) | `allocator.rs` → `AlignedAllocator::allocate()` |
| INV-ALLOC-02 | Compile-time `size_of`, [tla/RingSPSC.qnt](tla/RingSPSC.qnt) (`zeroOverheadDefault`) | N/A (ZST structural) |
| INV-INIT-01 | [tla/RingSPSC.qnt](tla/RingSPSC.qnt) (`initializedRange`), [tests/miri_tests.rs](tests/miri_tests.rs) | `invariants.rs` → `ring.rs`, `stack_ring.rs` |

---

## 9. Formal Specification (Quint)

The lock-free protocol is formally specified in [Quint](https://quint-lang.org/) for model checking. This complements the runtime `debug_assert!` checks and Loom tests. (The spec originated in TLA+; the `.tla`/`.cfg` pair was retired when Quint 0.32.0 gained the `leadsTo` temporal operator — see [docs/QUINT_0_32_UPGRADE.md](../../docs/QUINT_0_32_UPGRADE.md).)

**Location**: [tla/RingSPSC.qnt](tla/RingSPSC.qnt) (safety), [tla/RingSPSCLiveness.qnt](tla/RingSPSCLiveness.qnt) (liveness)

### Invariant Mapping

| Spec Invariant | Quint Element | Description |
|----------------|---------------|-------------|
| INV-SEQ-01 | `boundedCount` | `(tl - hd) <= CAPACITY` |
| INV-SEQ-02 | Action constraints | Tail/head only increase (monotonic) |
| INV-ORD-01 | `producerWrite` | Release store publishes writes |
| INV-ORD-02 | `consumerRefreshCache`, `consumerAdvance` | Acquire load synchronizes reads |
| INV-ORD-03 | `happensBefore` | `hd <= tl` (consumer never reads ahead) |
| INV-SW-01 | Structural | Producer actions only modify `tl`, `cached_head` |
| INV-SW-02 | Structural | Consumer actions only modify `hd`, `cached_tail` |
| INV-MEM-04 | `allocatorCapacityCorrect` | `buffer_capacity == CAPACITY` (allocator contract) |
| INV-ALLOC-01 | `alignmentGuarantee` | `buffer_aligned` flag (structural in Rust) |
| INV-ALLOC-02 | `zeroOverheadDefault` | `allocator_zst` flag (structural in Rust) |
| INV-INIT-01 | `initializedRange` | `initialized` set tracks slot init state |
| Liveness | `eventuallyConsumed` | All produced items are eventually consumed (under weak fairness) |

### Refinement Mapping (Quint → Rust)

| Quint Action | Rust Function |
|--------------|---------------|
| `producerReserveFast` | `ring.rs: reserve()` fast path |
| `producerRefreshCache` | `ring.rs: reserve()` slow path (Acquire load) |
| `producerWrite` | `ring.rs: commit_internal()` |
| `consumerRefreshCache` | `ring.rs: consume_batch()` slow path |
| `consumerAdvance` | `ring.rs: advance()` |

### Running the Model Checker

```bash
cd crates/ringmpsc/tla

# Exhaustive safety checking via TLC (955 states, requires Quint ≥ 0.31.0, JDK 21+)
quint verify RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant --backend=tlc

# Exhaustive liveness checking via TLC (requires Quint ≥ 0.32.0, JDK 21+)
quint verify RingSPSCLiveness.qnt --main=RingSPSCLiveness --temporal=eventuallyConsumed --backend=tlc

# Symbolic model checking via Apalache (complementary, requires JDK 21+)
quint verify RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant
```

See [tla/README.md](tla/README.md) for prerequisites and detailed instructions.

### Design Decision: Unbounded Integers

The Quint spec uses unbounded `int` instead of `u64` wrap-around because:
1. Invariants (bounded count, monotonicity) don't depend on overflow behavior
2. The model checker's state space is bounded by `MAX_ITEMS` anyway
3. Wrap-around correctness is a separate concern tested via Loom

At 10 billion msg/sec, `u64` wrap-around takes ~58 years—practically infinite.

### Property-Based Testing (Proptest)

The formal invariants are also encoded as proptest properties in [tests/property_tests.rs](tests/property_tests.rs):

| Quint Invariant | Proptest Function | Coverage |
|-----------------|-------------------|----------|
| `boundedCount` | `prop_bounded_count_ring`, `prop_bounded_count_stack_ring` | Ring, StackRing |
| Monotonic (action constraints) | `prop_monotonic_progress`, `prop_monotonic_progress_stack_ring` | Ring, StackRing |
| `happensBefore` | `prop_happens_before`, `prop_happens_before_stack_ring` | Ring, StackRing |
| INV-RES-01 | `prop_partial_reservation` | Ring |

Run with:
```bash
cargo test -p ringmpsc-rs --test property_tests --release
cargo test -p ringmpsc-rs --test property_tests --features stack-ring --release
```

### Quint Model-Based Testing

The [tests/quint_mbt.rs](tests/quint_mbt.rs) driver replays traces generated from [tla/RingSPSC.qnt](tla/RingSPSC.qnt) against the real `Ring<T>`. Independently, the spec embeds 18 `run` scenarios executed by `quint test` (their names carry a `Test` suffix because `quint test`'s default `--match` only selects names containing "Test"):

| Embedded Quint Test | Description |
|---------------------|-------------|
| `initSatisfiesInvariantTest` | Initial state satisfies all invariants |
| `producerWriteMaintainsBoundedCountTest` | INV-SEQ-01 after a write |
| `consumerAdvanceMaintainsHappensBeforeTest` | INV-ORD-03 after an advance |
| `fillToCapacityTest` | Fill ring to max |
| `cacheRefreshScenarioTest` | Stale cache recovery |
| `allocatorCapacityAtInitTest` | INV-MEM-04: buffer_capacity == CAPACITY at init |
| `allocatorCapacityStableAcrossOpsTest` | INV-MEM-04: capacity stable across ops |
| `alignmentAtInitTest` | INV-ALLOC-01: alignment flag set |
| `zeroOverheadAtInitTest` | INV-ALLOC-02: ZST flag set |
| `producerWriteInitializesSlotTest` | INV-INIT-01: write marks slot initialized |
| `consumerAdvanceUninitializesSlotTest` | INV-INIT-01: consume marks slot uninitialized |
| `initializedRangeWrapAroundTest` | INV-INIT-01: modular arithmetic after wrap |
| `emptyRingNoInitializedSlotsTest` | INV-INIT-01: empty ring has empty set |
| `allocatorInvariantsThroughCycleTest` | All invariants at every step of a cycle |
| `numaPlacementAtInitTest` | INV-NUMA-01: placement flag set |
| `numaFallbackAtInitTest` | INV-NUMA-02: fallback flag set |
| `numaPolicyAtInitTest` | INV-NUMA-03: determinism flag set |
| `numaInvariantsStableAcrossOpsTest` | NUMA invariants stable across ops |

Run with:
```bash
# Rust driver tests
cargo test -p ringmpsc-rs --test quint_mbt --features quint-mbt --release

# Quint CLI — embedded tests (Rust backend, default since Quint 0.31.0)
cd crates/ringmpsc/tla
quint test RingSPSC.qnt --main=RingSPSC

# Quint CLI — simulation with invariant checking
quint run RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant
```
