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

| Invariant | Test Coverage |
|-----------|--------------|
| INV-MEM-01 | Manual inspection (no runtime check possible) |
| INV-SEQ-* | [tests/integration_tests.rs](../tests/integration_tests.rs) |
| INV-INIT-* | [tests/miri_tests.rs](../tests/miri_tests.rs) (UB detection) |
| INV-SW-* | [tests/loom_tests.rs](../tests/loom_tests.rs) (exhaustive interleavings) |
| INV-ORD-* | [tests/loom_tests.rs](../tests/loom_tests.rs) |
| INV-DROP-* | [tests/miri_tests.rs](../tests/miri_tests.rs) + manual review |
