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

## 3. Memory Initialization Invariants

### INV-INIT-01: Initialized Range
```
buffer[i] is initialized  ⟺  head ≤ sequence(i) < tail
```
Slots in range `[head, tail)` contain valid `T` values written by the producer.

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
// Fast path (cached)
head.load(Relaxed)           // Only consumer writes head
cached_tail (UnsafeCell)     // No ordering needed - single writer

// Slow path (refresh cache)  
tail.load(Acquire)           // SYNCHRONIZES with producer's Release

// Advance
read_data_from_buffer()      // No ordering - protected by protocol
head.store(new_head, Release) // Publishes consumption to producer
```

### INV-ORD-03: Happens-Before Chain
```
producer.write(data) → producer.tail.store(Release)
    ↓ (synchronizes-with)
consumer.tail.load(Acquire) → consumer.read(data)
```

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

### INV-CH-02: Sequential Consumption
`Channel::consume_all()` polls rings sequentially on a single thread. No concurrent consumption of the same ring.

### INV-CH-03: Per-Producer FIFO
Messages from a single producer are received in send order. No global ordering across producers.

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
| INV-CH-03 | [tests/integration_tests.rs](tests/integration_tests.rs) | `invariants.rs` → `channel.rs`, `stack_channel.rs` |
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
