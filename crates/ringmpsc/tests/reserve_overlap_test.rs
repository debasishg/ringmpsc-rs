//! Regression test: a producer must not be able to hold two overlapping
//! reservations. With `reserve(&self)` both borrow-check, and because the
//! cursor does not advance until `commit()`, both are handed `&mut` slices
//! over the *same* slots.
//!
//! Single-threaded, so Miri runs it in well under a second.

use ringmpsc_rs::{Channel, Config};

#[test]
fn producer_cannot_hold_two_overlapping_reservations() {
    let channel = Channel::<u64>::new(Config::new(3, 1, false)); // 8 slots
    let mut producer = channel.register().unwrap();

    let mut r1 = producer.reserve(2).unwrap();
    assert_eq!(r1.len(), 2);
    assert!(r1.push(1).is_ok());
    assert!(r1.push(2).is_ok());
    r1.commit();

    // Only valid because the first reservation was committed and released.
    let mut r2 = producer.reserve(2).unwrap();
    assert_eq!(r2.len(), 2);
    assert!(r2.push(3).is_ok());
    assert!(r2.push(4).is_ok());
    r2.commit();

    let mut seen = Vec::new();
    channel.consumer().unwrap().consume_all(|v| seen.push(*v));
    assert_eq!(seen, vec![1, 2, 3, 4]);
}
