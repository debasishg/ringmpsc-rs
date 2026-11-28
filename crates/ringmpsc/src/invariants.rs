//! Debug assertion macros for ring buffer invariants.
//!
//! These macros provide runtime checks for the invariants documented in `spec.md`.
//! They are only active in debug builds (`#[cfg(debug_assertions)]`), so there is
//! zero overhead in release builds. The one exception is
//! `assert_head_not_past_tail`, which guards memory safety and stays on in
//! release builds.
//!
//! Used by both `Ring<T>` and `StackRing<T, N>`.

// =============================================================================
// INV-SEQ-01: Bounded Count
// =============================================================================

/// Assert that count does not exceed capacity.
///
/// **Invariant**: `0 ≤ (tail - head) ≤ capacity`
///
/// Used in: `commit_internal()` after computing `new_tail`
macro_rules! debug_assert_bounded_count {
    ($count:expr, $capacity:expr) => {
        debug_assert!(
            $count <= $capacity,
            "INV-SEQ-01 violated: count {} exceeds capacity {}",
            $count,
            $capacity
        )
    };
}

/// Assert that advancing head by `n` does not move it past tail.
///
/// **Invariant**: `n ≤ tail - head`, so `head ≤ tail` after the advance
///
/// This check enforces INV-ORD-03 (HappensBefore: `head ≤ tail`) which is also
/// implied by INV-SEQ-01 (BoundedCount: `0 ≤ (tail - head) ≤ capacity`).
///
/// Unlike the other macros here, this is a real `assert!` that stays on in
/// release builds. A head past tail is not just a broken invariant: the next
/// consume walks `[head, tail)` as if it were initialized, and reads unwritten
/// slots (#14). The check compares `n` with the available count rather than
/// `head + n` with `tail`, so an `n` large enough to wrap `head + n` around,
/// which would move head *backwards* and re-read consumed slots, fails too.
///
/// Used in: `advance()` before updating head
macro_rules! assert_head_not_past_tail {
    ($head:expr, $n:expr, $tail:expr) => {{
        let available = $tail.wrapping_sub($head);
        assert!(
            ($n as u64) <= available,
            "INV-ORD-03 violated: advancing head {} by {} past tail {} ({} available)",
            $head,
            $n,
            $tail,
            available
        )
    }};
}

// =============================================================================
// INV-SEQ-02: Monotonic Progress
// =============================================================================

/// Assert that a sequence number only increases (monotonic progress).
///
/// **Invariant**: `new_value ≥ old_value` (using wrapping comparison)
///
/// Used in: `commit_internal()` for tail, `advance()` for head
macro_rules! debug_assert_monotonic {
    ($name:literal, $old:expr, $new:expr) => {
        debug_assert!(
            $new >= $old,
            "INV-SEQ-02 violated: {} decreased from {} to {}",
            $name,
            $old,
            $new
        )
    };
}

// =============================================================================
// INV-SEQ-03: No Wrap-Around (extremely unlikely but detectable)
// =============================================================================

/// Assert that we haven't wrapped around u64 sequence space.
///
/// **Invariant**: At 10B msg/sec, wrap takes ~58 years. This detects bugs where
/// sequence jumps backwards unexpectedly (not due to normal wrapping arithmetic).
///
/// Note: This uses strict `>` rather than `>=` because `new > old` detects
/// wrap-around (where new would be < old due to overflow).
///
/// Used in: `commit_internal()` after incrementing tail
macro_rules! debug_assert_no_wrap {
    ($name:literal, $old:expr, $new:expr) => {
        // In debug mode, detect if we somehow wrapped u64 (should never happen
        // in practice, but catches bugs where sequence jumps incorrectly)
        debug_assert!(
            $new > $old || $old.wrapping_sub($new) > (1u64 << 32),
            "INV-SEQ-03 potential wrap detected: {} went from {} to {} (delta: {})",
            $name,
            $old,
            $new,
            $new.wrapping_sub($old)
        )
    };
}

// =============================================================================
// INV-INIT-01: Initialized Range Check
// =============================================================================

/// Assert that we're reading from an initialized slot.
///
/// **Invariant**: `buffer[i] is initialized ⟺ head ≤ sequence(i) < tail`
///
/// Used in: `consume_batch()` before `assume_init_read()`
macro_rules! debug_assert_initialized_read {
    ($pos:expr, $head:expr, $tail:expr) => {
        debug_assert!(
            $pos >= $head && $pos < $tail,
            "INV-INIT-01 violated: reading slot at seq {} outside initialized range [{}, {})",
            $pos,
            $head,
            $tail
        )
    };
}

// =============================================================================
// INV-RES-03: Pointer Validity
// =============================================================================

// INV-RES-03 (ring pointer validity) no longer has a runtime assertion.
//
// It used to be a `debug_assert_valid_ring_ptr!` null check in
// `Reservation::commit_n_unchecked()`. `Reservation` now stores its ring as
// `NonNull<Ring<T, A>>`, so a null pointer is unrepresentable and the check
// was provably dead - rustc said as much: "returned pointer of `as_ptr` call
// is never null, so checking it for null will always return false".
//
// The invariant is unchanged; only its enforcement moved, from a check that
// ran in debug builds to one the type system makes at compile time. Listed
// here so an audit against the spec still finds INV-RES-03.

// =============================================================================
// INV-CH-03: Per-Producer FIFO (consumption count tracking)
// =============================================================================

/// Assert monotonic consumption count for FIFO verification.
///
/// **Invariant**: Messages from a single producer are received in send order.
/// We verify this by tracking consumption count per producer.
///
/// Used in: `Channel::consume_all()` with `#[cfg(debug_assertions)]`
#[allow(unused_macros)]
macro_rules! debug_assert_fifo_count {
    ($producer_id:expr, $old_count:expr, $new_count:expr) => {
        debug_assert!(
            $new_count >= $old_count,
            "INV-CH-03 violated: producer {} consumption count went from {} to {}",
            $producer_id,
            $old_count,
            $new_count
        )
    };
}

// =============================================================================
// INV-CH-04: Registration Bound and Close
// =============================================================================

/// Assert that the registered producer count is within the limit.
///
/// **Invariant**: the count never exceeds `max_producers`, not even for an
/// instant, so `rings[..count]` is always in bounds.
///
/// Used in: `Registry::claim()`
macro_rules! debug_assert_registered_bound {
    ($count:expr, $max:expr) => {
        debug_assert!(
            $count <= $max,
            "INV-CH-04 violated: {} producers registered, limit {}",
            $count,
            $max
        )
    };
}

/// Assert that `max_producers` leaves the registry's CLOSED bit free.
///
/// **Invariant (INV-CH-04)**: the count shares a word with the CLOSED bit, so
/// it must stay below that bit or a full registry would read as closed.
///
/// Used in: `Registry::claim()`
macro_rules! debug_assert_max_fits {
    ($max:expr, $closed_bit:expr) => {
        debug_assert!(
            $max < $closed_bit,
            "INV-CH-04 violated: max_producers {} reaches the CLOSED bit",
            $max
        )
    };
}

// =============================================================================
// INV-CH-05: Lossless Close (send gates)
// =============================================================================

/// Assert that a producer id has a send gate.
///
/// **Invariant**: `SendGates` has one gate per producer id, so every id a
/// sender uses is below the gate count.
///
/// Used in: `GatesImpl::begin()` (`gate.rs`)
macro_rules! debug_assert_gate_exists {
    ($id:expr, $gates:expr) => {
        debug_assert!(
            $id < $gates,
            "INV-CH-05 violated: producer id {} has no gate ({} gates)",
            $id,
            $gates
        )
    };
}

/// Assert that a failed `begin` failed because the gates are closed.
///
/// **Invariant**: at most one section per producer id at a time. Opening a
/// second one would fail like a closed gate and be reported as a spurious
/// `Closed` on an open channel, so catch that misuse here.
///
/// Used in: `GatesImpl::begin()` (`gate.rs`)
macro_rules! debug_assert_gate_not_reentered {
    ($prev:expr, $closed_bit:expr) => {
        debug_assert!(
            $prev & $closed_bit != 0,
            "INV-CH-05 violated: a second send section opened for one producer id"
        )
    };
}

/// Assert that ending a send section matches one that was open.
///
/// **Invariant**: a gate shows a section from its open until its end, and the
/// consumer's final drain waits for every gate to show none. Ending a section
/// that wasn't open would mean a double end, which could let the consumer
/// drain before a commit.
///
/// Used in: `SectionImpl::release()` (`gate.rs`)
macro_rules! debug_assert_section_was_open {
    ($in_section_bit:expr) => {
        debug_assert!(
            $in_section_bit != 0,
            "INV-CH-05 violated: a send section ended while none was open"
        )
    };
}

// =============================================================================
// INV-ALLOC-01: Alignment Guarantee
// =============================================================================

/// Assert that a buffer pointer is aligned to the expected boundary.
///
/// **Invariant**: `AlignedAllocator<ALIGN>` produces pointers where
/// `ptr as usize % ALIGN == 0`.
///
/// Used in: `AlignedAllocator::allocate()` after computing the aligned pointer
#[allow(unused_macros)]
macro_rules! debug_assert_aligned {
    ($ptr:expr, $align:expr) => {
        debug_assert!(
            ($ptr as usize) % $align == 0,
            "INV-ALLOC-01 violated: pointer {:p} not aligned to {} bytes",
            $ptr,
            $align
        )
    };
}

// =============================================================================
// INV-ALLOC-02: Zero Overhead Default
// =============================================================================

/// Compile-time assertion that `HeapAllocator` is a zero-sized type.
///
/// **Invariant**: `Ring<T>` and `Ring<T, HeapAllocator>` have identical
/// layout. The allocator adds zero bytes.
///
/// This is a compile-time check, not a runtime assertion.
#[allow(unused_macros)]
macro_rules! static_assert_zst {
    ($ty:ty) => {
        const _: () = assert!(
            std::mem::size_of::<$ty>() == 0,
            "INV-ALLOC-02 violated: HeapAllocator is not a ZST"
        );
    };
}

// =============================================================================
// INV-MEM-02: Power-of-Two Capacity (slot index in bounds)
// =============================================================================

/// Assert that a buffer slot index is in bounds.
///
/// **Invariant**: indices are `seq & (capacity - 1)` with `capacity` a power
/// of two, so every index is `< capacity`.
///
/// Used in: `Ring::slot()`, `StackRing::slot_ptr()`
macro_rules! debug_assert_slot_in_bounds {
    ($idx:expr, $capacity:expr) => {
        debug_assert!(
            $idx < $capacity,
            "INV-MEM-02 violated: slot index {} out of bounds for capacity {}",
            $idx,
            $capacity
        )
    };
}

// =============================================================================
// Re-exports for crate-internal use
// =============================================================================

pub(crate) use debug_assert_bounded_count;
#[allow(unused_imports)]
pub(crate) use debug_assert_fifo_count;
pub(crate) use assert_head_not_past_tail;
pub(crate) use debug_assert_initialized_read;
pub(crate) use debug_assert_monotonic;
pub(crate) use debug_assert_no_wrap;
pub(crate) use debug_assert_registered_bound;
pub(crate) use debug_assert_section_was_open;
pub(crate) use debug_assert_gate_exists;
pub(crate) use debug_assert_gate_not_reentered;
pub(crate) use debug_assert_max_fits;
pub(crate) use debug_assert_slot_in_bounds;
#[allow(unused_imports)]
pub(crate) use debug_assert_aligned;
#[allow(unused_imports)]
pub(crate) use static_assert_zst;

// =============================================================================
// INV-NUMA-01: Memory Placement (Linux only)
// =============================================================================

/// Assert that a buffer was placed on the expected NUMA node.
///
/// **Invariant**: `NumaAllocator::allocate()` places memory on the requested
/// NUMA node (INV-NUMA-01). On Linux, reads `/proc/self/numa_maps` to verify.
/// No-op on non-Linux platforms.
///
/// Used in: `numa.rs` allocate path (debug builds only)
#[allow(unused_macros)]
macro_rules! debug_assert_numa_placement {
    ($ptr:expr, $node:expr) => {
        #[cfg(all(debug_assertions, target_os = "linux"))]
        {
            // Best-effort verification — read /proc/self/numa_maps and look
            // for the page containing $ptr. If found, verify it maps to $node.
            let addr = $ptr as usize;
            if let Ok(contents) = std::fs::read_to_string("/proc/self/numa_maps") {
                for line in contents.lines() {
                    // Lines: "<hex_addr> <policy> <details>..."
                    if let Some(hex) = line.split_whitespace().next() {
                        if let Ok(map_addr) = usize::from_str_radix(
                            hex.trim_start_matches("0x"),
                            16,
                        ) {
                            if map_addr == (addr & !0xFFF) {
                                // Look for "N<node>=<pages>" in the line
                                let expected = format!("N{}=", $node);
                                debug_assert!(
                                    line.contains(&expected),
                                    "INV-NUMA-01 violated: ptr {:p} expected on node {}, \
                                     but numa_maps says: {}",
                                    $ptr as *const u8,
                                    $node,
                                    line
                                );
                                break;
                            }
                        }
                    }
                }
            }
        }
    };
}

#[allow(unused_imports)]
pub(crate) use debug_assert_numa_placement;

// =============================================================================
// INV-DROP-01: Ring Cleanup
// =============================================================================

/// Assert that the drop loop stays within `[head, tail)` and capacity.
///
/// **Invariant**: `Ring::drop()` and `StackRing::drop()` iterate exactly
/// `count = tail - head` slots, each index bounded by the capacity mask.
///
/// Used in: `ring.rs` and `stack_ring.rs` `Drop` impls
macro_rules! debug_assert_drop_bounds {
    ($count:expr, $capacity:expr, $idx:expr) => {
        debug_assert!(
            $count <= $capacity,
            "INV-DROP-01 violated: drop count {} exceeds capacity {}",
            $count,
            $capacity
        );
        debug_assert!(
            $idx < $capacity,
            "INV-DROP-01 violated: drop index {} out of bounds (capacity {})",
            $idx,
            $capacity
        );
    };
}
pub(crate) use debug_assert_drop_bounds;

// =============================================================================
// INV-NUMA-02: Fallback Safety
// =============================================================================

/// Assert that the non-Linux `NumaAllocator` fallback returns a valid buffer.
///
/// **Invariant**: On non-NUMA platforms, `NumaAllocator::allocate()` delegates
/// to `HeapAllocator` and must return a buffer of exactly `capacity` slots.
///
/// Used in: `numa.rs` non-Linux `allocate()` path
#[allow(unused_macros)]
macro_rules! debug_assert_numa_fallback {
    ($buf:expr, $capacity:expr) => {
        debug_assert!(
            $buf.len() == $capacity,
            "INV-NUMA-02 violated: fallback buffer length {} != capacity {}",
            $buf.len(),
            $capacity
        );
    };
}
#[allow(unused_imports)]
pub(crate) use debug_assert_numa_fallback;
