//! Property-based tests derived from TLA+ invariants in tla/RingSPSC.tla
//!
//! These tests use proptest to verify that the same invariants hold in the
//! Rust implementation as in the formal specification.
//!
//! Coverage:
//! - Ring<T> (heap-allocated)
//! - StackRing<T, N> (stack-allocated, feature-gated)
//!
//! Both implementations share the same invariants from spec.md.

use proptest::prelude::*;
use ringmpsc_rs::{Config, Ring};
use std::mem::MaybeUninit;

#[cfg(feature = "stack-ring")]
use ringmpsc_rs::StackRing;

// =============================================================================
// INV-SEQ-01: Bounded Count
// "0 ≤ (tail - head) ≤ capacity"
// TLA+: BoundedCount == (tail - head) <= Capacity
// =============================================================================

proptest! {
    /// INV-SEQ-01: Ring never exceeds capacity after any sequence of operations
    #[test]
    fn prop_bounded_count_ring(
        writes in 0usize..100,
        reads in 0usize..100,
    ) {
        let config = Config::default(); // 64K capacity
        let ring = Ring::<u64>::new(config);
        let capacity = ring.capacity();

        // Write some items (bounded by capacity)
        let actual_writes = writes.min(capacity);
        for i in 0..actual_writes {
            if let Some(mut r) = ring.reserve(1) {
                r.as_mut_slice()[0] = MaybeUninit::new(i as u64);
                r.commit();
            }
        }

        // Invariant must hold after writes
        prop_assert!(ring.len() <= capacity,
            "INV-SEQ-01 violated after writes: len {} > capacity {}", ring.len(), capacity);

        // Read some items
        let mut read_count = 0;
        let _target_reads = reads.min(actual_writes);
        ring.consume_batch(|_| {
            read_count += 1;
        });

        // Invariant must hold after reads
        prop_assert!(ring.len() <= capacity,
            "INV-SEQ-01 violated after reads: len {} > capacity {}", ring.len(), capacity);

        // Additional check: we consumed what was available
        prop_assert!(read_count <= actual_writes,
            "Read more than written: {} > {}", read_count, actual_writes);
    }
}

#[cfg(feature = "stack-ring")]
proptest! {
    /// INV-SEQ-01: StackRing never exceeds capacity (same invariant, different impl)
    #[test]
    fn prop_bounded_count_stack_ring(
        writes in 0usize..100,
    ) {
        const CAP: usize = 64;
        let ring = StackRing::<u64, CAP>::new();

        // Write some items using unsafe API
        let actual_writes = writes.min(CAP);
        for i in 0..actual_writes {
            // SAFETY: We write exactly 1 item and commit 1
            unsafe {
                if let Some((ptr, len)) = ring.reserve(1) {
                    if len >= 1 {
                        std::ptr::write(ptr, i as u64);
                        ring.commit(1);
                    }
                }
            }
        }

        // Invariant must hold
        prop_assert!(ring.len() <= CAP,
            "INV-SEQ-01 violated: len {} > capacity {}", ring.len(), CAP);

        // Consume and verify again
        // SAFETY: Callback doesn't violate any invariants
        unsafe { ring.consume_batch(|_| {}); }
        prop_assert!(ring.len() <= CAP);
    }
}

// =============================================================================
// INV-SEQ-02: Monotonic Progress
// "head_new ≥ head_old, tail_new ≥ tail_old"
// TLA+: Encoded as action constraints (tail/head only increase)
// =============================================================================

proptest! {
    /// INV-SEQ-02: len() changes predictably - increases on write, decreases on consume
    #[test]
    fn prop_monotonic_progress(
        ops in prop::collection::vec(prop::bool::ANY, 1..50),
    ) {
        let ring = Ring::<u64>::new(Config::default());

        for write_op in ops {
            let len_before = ring.len();

            if write_op {
                // Write operation: len should increase or stay same (if full)
                if let Some(mut r) = ring.reserve(1) {
                    r.as_mut_slice()[0] = MaybeUninit::new(42);
                    r.commit();
                    let len_after = ring.len();
                    prop_assert!(len_after == len_before + 1,
                        "INV-SEQ-02: len didn't increase after successful write: {} -> {}",
                        len_before, len_after);
                }
                // If reserve failed (full), len stays same - that's fine
            } else {
                // Read operation: len should decrease or stay same (if empty)
                let consumed = ring.consume_batch(|_| {});
                let len_after = ring.len();
                if consumed > 0 {
                    prop_assert!(len_after < len_before,
                        "INV-SEQ-02: len didn't decrease after consume: {} -> {} (consumed {})",
                        len_before, len_after, consumed);
                }
            }
        }
    }
}

#[cfg(feature = "stack-ring")]
proptest! {
    /// INV-SEQ-02: StackRing monotonic progress
    #[test]
    fn prop_monotonic_progress_stack_ring(
        ops in prop::collection::vec(prop::bool::ANY, 1..30),
    ) {
        const CAP: usize = 32;
        let ring = StackRing::<u64, CAP>::new();

        for write_op in ops {
            let len_before = ring.len();

            if write_op {
                // SAFETY: We write exactly 1 item and commit 1
                unsafe {
                    if let Some((ptr, len)) = ring.reserve(1) {
                        if len >= 1 {
                            std::ptr::write(ptr, 42u64);
                            ring.commit(1);
                            let len_after = ring.len();
                            prop_assert!(len_after == len_before + 1);
                        }
                    }
                }
            } else {
                // SAFETY: Callback doesn't violate any invariants
                let consumed = unsafe { ring.consume_batch(|_| {}) };
                let len_after = ring.len();
                if consumed > 0 {
                    prop_assert!(len_after < len_before);
                }
            }
        }
    }
}

// =============================================================================
// INV-ORD-03: Happens-Before
// "head <= tail" (consumer never reads ahead of producer)
// TLA+: HappensBefore == head <= tail
// =============================================================================

proptest! {
    /// INV-ORD-03: Cannot consume more than was produced
    #[test]
    fn prop_happens_before(
        writes in 0usize..50,
    ) {
        let ring = Ring::<u64>::new(Config::default());

        // Write items
        let mut produced = 0;
        for i in 0..writes {
            if let Some(mut r) = ring.reserve(1) {
                r.as_mut_slice()[0] = MaybeUninit::new(i as u64);
                r.commit();
                produced += 1;
            }
        }

        // Verify len matches produced
        prop_assert_eq!(ring.len(), produced,
            "len {} != produced {}", ring.len(), produced);

        // Consume all
        let mut consumed = 0;
        ring.consume_batch(|_| consumed += 1);

        // Cannot consume more than produced
        prop_assert!(consumed <= produced,
            "INV-ORD-03: consumed {} > produced {}", consumed, produced);

        // Ring should be empty after consuming all
        prop_assert!(ring.is_empty(),
            "INV-ORD-03: ring not empty after consuming all (len={})", ring.len());
    }
}

#[cfg(feature = "stack-ring")]
proptest! {
    /// INV-ORD-03: StackRing happens-before
    #[test]
    fn prop_happens_before_stack_ring(
        writes in 0usize..30,
    ) {
        const CAP: usize = 32;
        let ring = StackRing::<u64, CAP>::new();

        let mut produced = 0;
        for i in 0..writes.min(CAP) {
            // SAFETY: We write exactly 1 item and commit 1
            unsafe {
                if let Some((ptr, len)) = ring.reserve(1) {
                    if len >= 1 {
                        std::ptr::write(ptr, i as u64);
                        ring.commit(1);
                        produced += 1;
                    }
                }
            }
        }

        let mut consumed = 0;
        // SAFETY: Callback doesn't violate any invariants
        unsafe { ring.consume_batch(|_| consumed += 1); }

        prop_assert!(consumed <= produced);
        prop_assert!(ring.is_empty());
    }
}

// =============================================================================
// INV-RES-01: Partial Reservation (wrap-around behavior)
// "reserve(n) may return len() < n due to buffer wrap-around"
// =============================================================================

proptest! {
    /// INV-RES-01: Reservation length is bounded by request and available space
    #[test]
    fn prop_partial_reservation(
        request_size in 1usize..100,
        pre_fill in 0usize..50,
    ) {
        let config = Config::new(6, 1, false); // 64 capacity, 1 producer
        let ring = Ring::<u64>::new(config);
        let capacity = ring.capacity();

        // Pre-fill some slots
        let actual_fill = pre_fill.min(capacity);
        for i in 0..actual_fill {
            if let Some(mut r) = ring.reserve(1) {
                r.as_mut_slice()[0] = MaybeUninit::new(i as u64);
                r.commit();
            }
        }

        // Request reservation
        let available = capacity - ring.len();
        if let Some(r) = ring.reserve(request_size) {
            let got = r.len();
            // Got <= requested (partial reservation)
            prop_assert!(got <= request_size,
                "INV-RES-01: got {} > requested {}", got, request_size);
            // Got <= available space
            prop_assert!(got <= available,
                "INV-RES-01: got {} > available {}", got, available);
            // Got > 0 (if we got a reservation)
            prop_assert!(got > 0, "INV-RES-01: empty reservation");
            // Don't commit - let it drop
        }
    }
}
