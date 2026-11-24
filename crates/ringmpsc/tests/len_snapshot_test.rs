//! `len()` / `is_empty()` / `is_full()` called from a thread that is neither
//! the producer nor the consumer, while both run.
//!
//! Such an observer reads `head` and `tail` with two separate loads, and both
//! move in between. `len()` must still return a value in `0..=capacity`:
//! it used to load `tail` before `head`, so an observer could see an old
//! `tail` with a newer `head`, and `tail - head` wrapped to about `2^64`.
//! `is_full()` then reported a nearly empty ring as full.

use ringmpsc_rs::{Channel, Config};
#[cfg(feature = "stack-ring")]
use ringmpsc_rs::StackChannel;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

/// Items pushed through the ring per test. Enough for the observer to
/// catch the producer and consumer mid-move many times.
const ITEMS: u64 = 200_000;

/// Runs `produce` and `consume` concurrently with an observer that calls
/// `observe` until both finish. Returns the number of observations.
fn race(
    produce: impl FnOnce() + Send,
    consume: impl FnOnce() + Send,
    observe: impl Fn() + Sync,
) -> u64 {
    let done = AtomicBool::new(false);
    thread::scope(|s| {
        let observer = s.spawn(|| {
            let mut observations = 0u64;
            while !done.load(Ordering::Acquire) {
                observe();
                observations += 1;
            }
            observations
        });
        let producer = s.spawn(produce);
        let consumer = s.spawn(consume);
        producer.join().expect("producer panicked");
        consumer.join().expect("consumer panicked");
        done.store(true, Ordering::Release);
        observer.join().expect("observer: len() out of range")
    })
}

#[test]
fn ring_len_stays_within_capacity_under_concurrency() {
    let channel = Channel::<u64>::new(Config::new(3, 1, false)); // 8 slots
    let observations = race(
        || {
            let producer = channel.register().expect("one producer");
            let mut i = 0;
            while i < ITEMS {
                if producer.push(i) {
                    i += 1;
                } else {
                    thread::yield_now();
                }
            }
        },
        || {
            let mut consumer = channel.consumer().expect("one consumer");
            let mut seen = 0u64;
            while seen < ITEMS {
                seen += consumer.consume_all(|_| {}) as u64;
            }
        },
        || {
            if let Some(ring) = channel.get_ring(0) {
                let len = ring.len();
                assert!(len <= ring.capacity(), "len() = {len} > capacity {}", ring.capacity());
            }
        },
    );
    assert!(observations > 0);
}

#[cfg(feature = "stack-ring")]
#[test]
fn stack_ring_len_stays_within_capacity_under_concurrency() {
    let channel: StackChannel<u64, 8, 1> = StackChannel::new();
    let observations = race(
        || {
            let producer = channel.register().expect("one producer");
            let mut i = 0;
            while i < ITEMS {
                if producer.push(i) {
                    i += 1;
                } else {
                    thread::yield_now();
                }
            }
        },
        || {
            let mut consumer = channel.consumer().expect("one consumer");
            let mut seen = 0u64;
            while seen < ITEMS {
                seen += consumer.consume_all(|_| {}) as u64;
            }
        },
        || {
            if let Some(ring) = channel.get_ring(0) {
                let len = ring.len();
                assert!(len <= ring.capacity(), "len() = {len} > capacity {}", ring.capacity());
            }
        },
    );
    assert!(observations > 0);
}
