//! Debug assertion macros for ring buffer invariants.
//!
//! These macros provide runtime checks for the invariants documented in `spec.md`.
//! They are only active in debug builds (`#[cfg(debug_assertions)]`), so there is
//! zero overhead in release builds.
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

/// Assert that head does not advance past tail.
///
/// **Invariant**: `head ≤ tail` (after advance)
///
/// Used in: `advance()` before updating head
macro_rules! debug_assert_head_not_past_tail {
    ($new_head:expr, $tail:expr) => {
        debug_assert!(
            $new_head <= $tail,
            "INV-SEQ-01 violated: advancing head {} beyond tail {}",
            $new_head,
            $tail
        )
    };
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

/// Assert that a ring pointer is not null.
///
/// **Invariant**: The raw `ring_ptr` in `Reservation` is valid for lifetime `'a`
///
/// Used in: `Reservation::commit_n_unchecked()`
macro_rules! debug_assert_valid_ring_ptr {
    ($ptr:expr) => {
        debug_assert!(
            !$ptr.is_null(),
            "INV-RES-03 violated: null ring pointer"
        )
    };
}

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
// Re-exports for crate-internal use
// =============================================================================

pub(crate) use debug_assert_bounded_count;
#[allow(unused_imports)]
pub(crate) use debug_assert_fifo_count;
pub(crate) use debug_assert_head_not_past_tail;
pub(crate) use debug_assert_initialized_read;
pub(crate) use debug_assert_monotonic;
pub(crate) use debug_assert_no_wrap;
pub(crate) use debug_assert_valid_ring_ptr;
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
