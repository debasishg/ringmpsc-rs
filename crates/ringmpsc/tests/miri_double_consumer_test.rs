//! Two consumers on one ring, now unreachable from safe code.
//!
//! `Channel` used to consume through `&self` while being `Clone + Sync`, so two
//! threads could drain the same ring at once. Each loaded the same head, moved
//! the same slot out with `assume_init_read`, and one `Box` ended up owned
//! twice. Commit 100122f has the repro, which Miri rejects under both Stacked
//! and Tree Borrows. It no longer compiles: consumption now goes through
//! `Channel::consumer()`, which hands out at most one `Consumer`, and every
//! consume needs `&mut Consumer`.
//!
//! These tests drive the same overlap through the new API and check that the
//! claim protocol holds it apart, natively and under Miri:
//!
//! ```text
//! cargo +nightly miri test --test miri_double_consumer_test
//!
//! MIRIFLAGS="-Zmiri-tree-borrows -Zmiri-preemption-rate=0.1" \
//!   cargo +nightly miri test --test miri_double_consumer_test
//! ```

use ringmpsc_rs::{Channel, ChannelError, Config};
use std::sync::Barrier;
use std::thread;

/// The original repro's interleaving: the first consumer is parked inside its
/// handler, holding an item it has moved out but before it stores the new
/// head. The second thread now cannot get a consumer at that point.
#[test]
fn second_consumer_is_refused_while_first_holds_an_item() {
    let channel = Channel::<Box<u64>>::new(Config::new(2, 1, false));
    let producer = channel.register().unwrap();
    assert!(producer.push(Box::new(42)));

    let other = channel.clone();
    let first_holds_the_item = Barrier::new(2);
    let second_has_tried = Barrier::new(2);

    thread::scope(|s| {
        s.spawn(|| {
            let mut consumer = channel.consumer().unwrap();
            let consumed = consumer.consume_all_owned(|item| {
                first_holds_the_item.wait();
                second_has_tried.wait();
                assert_eq!(*item, 42);
            });
            assert_eq!(consumed, 1);
        });
        s.spawn(|| {
            first_holds_the_item.wait();
            assert_eq!(other.consumer().err(), Some(ChannelError::ConsumerTaken));
            second_has_tried.wait();
        });
    });

    // The item was moved out exactly once, so nothing is left to drain.
    assert_eq!(channel.consumer().unwrap().consume_all_owned(drop), 0);
}

/// Consumption can move between threads, one consumer at a time. Nothing
/// joins the first thread before the second starts consuming, so the only
/// thing ordering them is the claim itself: the Release in `Consumer::drop`
/// and the Acquire in `Channel::consumer`. With both weakened to `Relaxed`,
/// Miri reports a data race on the ring's consumer-owned `cached_tail`, which
/// `recv` reads and writes, on most schedules. Use `-Zmiri-many-seeds` to
/// sweep them.
#[test]
fn consumer_hands_off_between_threads() {
    let channel = Channel::<u64>::new(Config::new(2, 1, false));
    let producer = channel.register().unwrap();
    for i in 0..4 {
        assert!(producer.push(i));
    }

    let mut first = channel.consumer().unwrap();
    let other = channel.clone();

    thread::scope(|s| {
        s.spawn(move || {
            let mut out = [0u64; 2];
            assert_eq!(first.recv(&mut out), 2);
            assert_eq!(out, [0, 1]);
            // `first` drops here, releasing the claim.
        });
        s.spawn(|| {
            let mut consumer = loop {
                match other.consumer() {
                    Ok(c) => break c,
                    Err(ChannelError::ConsumerTaken) => thread::yield_now(),
                    Err(e) => panic!("unexpected error: {e}"),
                }
            };
            let mut out = [0u64; 4];
            let n = consumer.recv(&mut out);
            // Resumes after what the first consumer took: nothing seen twice.
            assert_eq!(&out[..n], &[2, 3]);
        });
    });
}
