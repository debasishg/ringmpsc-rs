//! Runtime checks of the verified MPSC channel, compiled as ordinary Rust
//! (the ghost code is erased), on real threads, natively and under Miri:
//!
//! ```text
//! CARGO_TARGET_DIR=target/plain cargo test
//! CARGO_TARGET_DIR=target/miri MIRIFLAGS="-Zmiri-preemption-rate=0.1" cargo +nightly miri test
//! ```

use ringmpsc_verified::channel::Channel;
use std::thread;

const PRODUCERS: usize = 4;
/// Fewer items under Miri, which interprets every step.
const PER_PRODUCER: u64 = if cfg!(miri) { 60 } else { 20_000 };

#[test]
fn concurrent_registration_hands_out_distinct_rings() {
    let channel = Channel::<u64>::new(2, PRODUCERS);
    let handles: Vec<_> = (0..PRODUCERS + 2)
        .map(|_| {
            let ch = channel.clone_handle();
            thread::spawn(move || ch.register().map(|(id, _producer)| id))
        })
        .collect();
    let mut ids: Vec<usize> = handles
        .into_iter()
        .filter_map(|h| h.join().unwrap())
        .collect();
    ids.sort_unstable();
    // Exactly PRODUCERS registrations succeed, each with its own ring.
    assert_eq!(ids, (0..PRODUCERS).collect::<Vec<_>>());
    assert!(channel.register().is_none());
}

#[test]
fn per_producer_fifo_across_threads() {
    let channel = Channel::<(usize, u64)>::new(3, PRODUCERS);
    let mut consumer = channel.consumer().expect("first claim");

    let producers: Vec<_> = (0..PRODUCERS)
        .map(|_| {
            let ch = channel.clone_handle();
            thread::spawn(move || {
                let (id, mut p) = ch.register().expect("a free ring");
                for seq in 0..PER_PRODUCER {
                    let mut item = (id, seq);
                    while let Err(back) = p.push(item) {
                        item = back;
                        thread::yield_now();
                    }
                }
            })
        })
        .collect();

    let mut next = vec![0u64; PRODUCERS];
    let total = PRODUCERS as u64 * PER_PRODUCER;
    let mut received = 0u64;
    while received < total {
        let batch = consumer.consume_all(5);
        if batch.is_empty() {
            thread::yield_now();
        }
        for (id, seq) in batch {
            assert_eq!(seq, next[id], "producer {id}'s items arrived out of order");
            next[id] += 1;
            received += 1;
        }
    }
    for p in producers {
        p.join().unwrap();
    }
    assert_eq!(next, vec![PER_PRODUCER; PRODUCERS]);
}

#[test]
fn consumer_claim_is_exclusive_and_released_on_drop() {
    let channel = Channel::<u64>::new(2, 2);
    let other = channel.clone_handle();

    let first = channel.consumer().expect("first claim");
    assert!(other.consumer().is_none(), "a second consumer while the first exists");

    // Release on another thread; the claim then succeeds here.
    thread::spawn(move || drop(first)).join().unwrap();
    let (_, mut p) = other.register().unwrap();
    assert!(p.push(7).is_ok());
    let mut again = other.consumer().expect("claim after release");
    assert_eq!(again.consume_all(10), vec![7]);
}

/// Counts drops, to check that the channel drops what it still owns.
struct Counted(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// When the last handle goes away with items still in the rings and no
/// consumer claimed, the items are dropped, not leaked. Under Miri this is
/// also a leak check: the unregistered producer halves and the unclaimed
/// consumer halves must be freed too.
#[test]
fn dropping_the_channel_drops_remaining_items() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let drops = std::sync::Arc::new(AtomicUsize::new(0));
    let channel = Channel::<Counted>::new(2, 3); // one ring stays unregistered
    let (_, mut p0) = channel.register().unwrap();
    let (_, mut p1) = channel.register().unwrap();
    for _ in 0..2 {
        assert!(p0.push(Counted(drops.clone())).is_ok());
        assert!(p1.push(Counted(drops.clone())).is_ok());
    }
    drop(p0);
    drop(p1);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(channel);
    assert_eq!(drops.load(Ordering::SeqCst), 4);
}
