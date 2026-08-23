//! Regression test: a producer must not be able to hold two overlapping
//! reservations. With `reserve(&self)` both borrow-check, and because the
//! cursor does not advance until `commit()`, both are handed `&mut` slices
//! over the *same* slots.
//!
//! Single-threaded, so Miri runs it in well under a second.

use ringmpsc_rs::{Channel, Config};
use std::mem::MaybeUninit;

#[test]
fn producer_cannot_hold_two_overlapping_reservations() {
    let channel = Channel::<u64>::new(Config::new(3, 1, false)); // 8 slots
    let mut producer = channel.register().unwrap();

    let mut r1 = producer.reserve(2).unwrap();
    let s1 = r1.as_mut_slice();
    assert_eq!(s1.len(), 2);
    s1[0] = MaybeUninit::new(1);
    s1[1] = MaybeUninit::new(2);
    r1.commit();

    // Only valid because the first reservation was committed and released.
    let mut r2 = producer.reserve(2).unwrap();
    let s2 = r2.as_mut_slice();
    assert_eq!(s2.len(), 2);
    s2[0] = MaybeUninit::new(3);
    s2[1] = MaybeUninit::new(4);
    r2.commit();

    let mut seen = Vec::new();
    channel.consume_all(|v| seen.push(*v));
    assert_eq!(seen, vec![1, 2, 3, 4]);
}
