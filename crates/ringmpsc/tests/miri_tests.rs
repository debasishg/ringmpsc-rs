//! Miri-compatible tests for detecting undefined behavior.
//!
//! Run with: `cargo +nightly miri test --test miri_tests`
//!
//! Miri is an interpreter for Rust's MIR that detects undefined behavior:
//! - Use of uninitialized memory
//! - Out-of-bounds memory access  
//! - Use-after-free
//! - Invalid pointer alignment
//! - Data races (with -Zmiri-check-number-validity)
//!
//! These tests are designed to exercise the unsafe code paths in ringmpsc-rs.

use ringmpsc_rs::{Channel, Config, Ring};
use std::mem::MaybeUninit;

/// Test basic ring buffer operations for UB.
#[test]
fn miri_ring_basic_operations() {
    let config = Config::new(4, 1, false); // Small ring for faster miri execution
    let mut ring = Ring::<u64>::new(config);

    // Reserve and commit
    if let Some(mut reservation) = ring.reserve(2) {
        let slice = reservation.as_mut_slice();
        slice[0] = MaybeUninit::new(100);
        slice[1] = MaybeUninit::new(200);
        reservation.commit();
    }

    // Consume
    let mut sum = 0u64;
    ring.consume_batch(|item| {
        sum += *item;
    });
    assert_eq!(sum, 300);
}

/// Test wrap-around behavior for UB.
#[test]
fn miri_ring_wrap_around() {
    let config = Config::new(2, 1, false); // capacity = 4
    let mut ring = Ring::<u32>::new(config);

    // Fill and drain multiple times to exercise wrap-around
    for round in 0..3 {
        // Fill
        for i in 0..4 {
            assert!(ring.push(round * 10 + i), "push failed at round {round} item {i}");
        }
        
        // Drain
        let mut count = 0;
        ring.consume_batch(|_item| {
            count += 1;
        });
        assert_eq!(count, 4);
    }
}

/// Test reservation that wraps around buffer boundary.
#[test]
fn miri_ring_partial_reservation() {
    let config = Config::new(2, 1, false); // capacity = 4
    let mut ring = Ring::<u64>::new(config);

    // Fill 3 slots
    for i in 0..3 {
        assert!(ring.push(i));
    }

    // Consume 2 to move head
    let mut consumed = 0;
    ring.consume_up_to(2, |_| consumed += 1);
    assert_eq!(consumed, 2);

    // Now head=2, tail=3. Reserving 3 should give us partial (only 1 slot contiguous before wrap)
    if let Some(mut res) = ring.reserve(3) {
        let len = res.as_mut_slice().len();
        // Should get 1 or 2, not 3 (depends on internal state)
        assert!(len <= 2, "Expected partial reservation, got {len}");
        for slot in res.as_mut_slice().iter_mut() {
            *slot = MaybeUninit::new(999);
        }
        res.commit();
    }
}

/// Test channel with multiple producers for UB.
#[test]
fn miri_channel_multi_producer() {
    let config = Config::new(4, 4, false);
    let channel = Channel::<u64>::new(config);

    // Register multiple producers
    let p1 = channel.register().unwrap();
    let p2 = channel.register().unwrap();

    // Send from both
    assert!(p1.push(1));
    assert!(p1.push(2));
    assert!(p2.push(10));
    assert!(p2.push(20));

    // Consume all
    let mut sum = 0u64;
    channel.consumer().unwrap().consume_all(|item| sum += *item);
    assert_eq!(sum, 33);
}

/// Test Drop behavior with unconsumed items.
#[test]
fn miri_ring_drop_with_items() {
    let config = Config::new(4, 1, false);
    
    {
        let mut ring = Ring::<String>::new(config);
        
        // Push some Strings (have Drop impl)
        if let Some(mut res) = ring.reserve(2) {
            let slice = res.as_mut_slice();
            slice[0] = MaybeUninit::new(String::from("hello"));
            slice[1] = MaybeUninit::new(String::from("world"));
            res.commit();
        }
        
        // Consume only one
        let mut received = Vec::new();
        ring.consume_up_to(1, |item| {
            received.push(item.clone());
        });
        assert_eq!(received.len(), 1);
        
        // Ring drops here with one unconsumed String
        // Miri will catch if Drop isn't called properly
    }
}

/// Test reservation dropped without commit.
#[test]
fn miri_reservation_drop_without_commit() {
    let config = Config::new(4, 1, false);
    let mut ring = Ring::<u64>::new(config);

    // Reserve but don't commit
    {
        let reservation = ring.reserve(2);
        assert!(reservation.is_some());
        // Reservation drops without commit - should not publish anything
    }

    // Ring should still be empty (nothing was committed)
    assert!(ring.is_empty());
}

/// Test `consume_up_to` boundary conditions.
#[test]
fn miri_consume_up_to_limits() {
    let config = Config::new(4, 1, false);
    let mut ring = Ring::<u64>::new(config);

    // Push 3 items
    for i in 0..3 {
        assert!(ring.push(i));
    }

    // Consume 0 (edge case)
    let count = ring.consume_up_to(0, |_| {});
    assert_eq!(count, 0);

    // Consume more than available
    let mut items = Vec::new();
    let count = ring.consume_up_to(100, |item| items.push(*item));
    assert_eq!(count, 3);
    assert_eq!(items, vec![0, 1, 2]);
}

/// Test the `push()` convenience method for UB.
#[test]
fn miri_push_convenience() {
    let config = Config::new(2, 1, false); // capacity = 4
    let mut ring = Ring::<u64>::new(config);

    // Push until full
    assert!(ring.push(1));
    assert!(ring.push(2));
    assert!(ring.push(3));
    assert!(ring.push(4));
    
    // Should fail when full
    assert!(!ring.push(5));

    // Verify all items
    let mut sum = 0;
    ring.consume_batch(|item| sum += *item);
    assert_eq!(sum, 10);
}

// =============================================================================
// #17: `cached_tail` can trail `head`
//
// `consume_batch*`, `consume_up_to*` and a bare `advance` move `head` without
// refreshing the consumer's `cached_tail`. `readable` (and `recv`, which uses
// it) used to compute `cached_tail.wrapping_sub(head)`, which wraps to a huge
// count when `cached_tail < head`, and then returned a slice over unwritten
// slots. Each test below leaves `cached_tail < head` and checks that the next
// read sees exactly what was published.
// =============================================================================

/// After `consume_batch` drains the ring, `readable` must see it empty.
#[test]
fn miri_readable_after_consume_batch() {
    let mut ring = Ring::<u64>::new(Config::new(2, 1, false)); // capacity = 4
    assert!(ring.push(7));
    assert_eq!(ring.consume_batch(|_| {}), 1); // head = 1, cached_tail = 0

    assert!(ring.readable().is_none(), "empty ring must have nothing readable");

    // Newly published items are seen, and only those.
    assert!(ring.push(8));
    assert!(ring.push(9));
    assert_eq!(ring.readable(), Some(&[8u64, 9][..]));
}

/// `consume_up_to` leaves items behind; `readable` must see only those.
#[test]
fn miri_readable_after_consume_up_to() {
    let mut ring = Ring::<u64>::new(Config::new(2, 1, false));
    for v in [1, 2, 3] {
        assert!(ring.push(v));
    }
    assert_eq!(ring.consume_up_to(2, |_| {}), 2); // head = 2, cached_tail = 0

    assert_eq!(ring.readable(), Some(&[3u64][..]));
}

/// `advance` can skip published items without the consumer ever loading
/// `tail`; `readable` must still see the ring as empty afterwards.
#[test]
fn miri_readable_after_bare_advance() {
    let mut ring = Ring::<u64>::new(Config::new(2, 1, false));
    assert!(ring.push(1));
    assert!(ring.push(2));
    ring.advance(2); // head = 2, cached_tail = 0

    assert!(ring.readable().is_none());
}

/// `recv` goes through `readable`, so it hit the same bug.
#[test]
fn miri_recv_after_consume_batch() {
    let mut ring = Ring::<u64>::new(Config::new(2, 1, false));
    assert!(ring.push(7));
    ring.consume_batch(|_| {});

    let mut out = [0u64; 4];
    assert_eq!(ring.recv(&mut out), 0);

    assert!(ring.push(8));
    assert_eq!(ring.recv(&mut out), 1);
    assert_eq!(out[0], 8);
}
