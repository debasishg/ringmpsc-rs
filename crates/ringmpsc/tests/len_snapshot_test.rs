//! `len()` called from a thread that is neither the producer nor the
//! consumer, while both run. `is_full()` is `len() >= capacity()`, so it is
//! covered too.
//!
//! Such an observer reads `head` and `tail` with separate loads, and both move
//! in between. `len()` used to load `tail`, then `head`, so it could pair an
//! old `tail` with a newer `head`: `tail - head` wrapped to about `2^64`, and
//! `is_full()` reported a nearly empty ring as full. Pairing a new `tail` with
//! an old `head` gives more than the capacity instead.
//!
//! `len()` doesn't clamp, so `len() <= capacity()` is a real check: it fails
//! for both bad pairings.

use ringmpsc_rs::{Channel, Config};
#[cfg(feature = "stack-ring")]
use ringmpsc_rs::StackChannel;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

/// Items pushed through the ring per test. Enough for the observer to
/// catch the producer and consumer mid-move many times.
const ITEMS: u64 = 200_000;

/// Stores `true` when dropped, including while unwinding from a panic.
struct SetOnDrop<'a>(&'a AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Stores `true` only if dropped while unwinding from a panic.
struct SetOnPanic<'a>(&'a AtomicBool);

impl Drop for SetOnPanic<'_> {
    fn drop(&mut self) {
        if thread::panicking() {
            self.0.store(true, Ordering::Release);
        }
    }
}

/// Runs `produce` and `consume` concurrently with an observer that calls
/// `observe` until both finish, and at least once. Returns the number of
/// observations.
///
/// `produce` and `consume` get an `abort` flag, set if the other one panics,
/// and must return when it is set: otherwise a consumer would wait forever
/// for items a panicked producer never sends, or a producer for space a
/// panicked consumer never frees. The observer stops once both have returned
/// or panicked, so a panic fails the test instead of hanging it.
fn race(
    produce: impl FnOnce(&AtomicBool) + Send,
    consume: impl FnOnce(&AtomicBool) + Send,
    observe: impl Fn() + Sync,
) -> u64 {
    let done = AtomicBool::new(false);
    let abort = AtomicBool::new(false);
    thread::scope(|s| {
        let observer = s.spawn(|| {
            // Observe at least once, even if the producer and consumer finish
            // before this thread first runs (a slow or single-core machine).
            let mut observations = 0u64;
            loop {
                observe();
                observations += 1;
                if done.load(Ordering::Acquire) {
                    break;
                }
            }
            observations
        });
        // Set `done` however this closure exits, so the observer stops.
        let stop_observer = SetOnDrop(&done);
        let producer = s.spawn(|| {
            let _guard = SetOnPanic(&abort);
            produce(&abort);
        });
        let consumer = s.spawn(|| {
            let _guard = SetOnPanic(&abort);
            consume(&abort);
        });
        let produced = producer.join();
        let consumed = consumer.join();
        drop(stop_observer);
        let observations = observer.join().expect("observer: len() out of range");
        produced.expect("producer panicked");
        consumed.expect("consumer panicked");
        observations
    })
}

fn len_in_range(len: usize, capacity: usize) {
    assert!(len <= capacity, "len() = {len} > capacity {capacity}");
}

#[test]
fn ring_len_stays_within_capacity_under_concurrency() {
    let channel = Channel::<u64>::new(Config::new(3, 1, false)); // 8 slots
    let observations = race(
        |abort| {
            let producer = channel.register().expect("one producer");
            let mut i = 0;
            while i < ITEMS && !abort.load(Ordering::Acquire) {
                if producer.push(i) {
                    i += 1;
                } else {
                    thread::yield_now();
                }
            }
        },
        |abort| {
            let mut consumer = channel.consumer().expect("one consumer");
            let mut seen = 0u64;
            while seen < ITEMS && !abort.load(Ordering::Acquire) {
                seen += consumer.consume_all(|_| {}) as u64;
            }
        },
        || {
            if let Some(ring) = channel.get_ring(0) {
                len_in_range(ring.len(), ring.capacity());
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
        |abort| {
            let producer = channel.register().expect("one producer");
            let mut i = 0;
            while i < ITEMS && !abort.load(Ordering::Acquire) {
                if producer.push(i) {
                    i += 1;
                } else {
                    thread::yield_now();
                }
            }
        },
        |abort| {
            let mut consumer = channel.consumer().expect("one consumer");
            let mut seen = 0u64;
            while seen < ITEMS && !abort.load(Ordering::Acquire) {
                seen += consumer.consume_all(|_| {}) as u64;
            }
        },
        || {
            if let Some(ring) = channel.get_ring(0) {
                len_in_range(ring.len(), ring.capacity());
            }
        },
    );
    assert!(observations > 0);
}
