//! Runtime checks of the verified concurrent ring, compiled as ordinary
//! Rust (the ghost code is erased). The proofs cover every interleaving;
//! these run real threads, natively and under Miri:
//!
//! ```text
//! CARGO_TARGET_DIR=target/plain cargo test
//! CARGO_TARGET_DIR=target/miri MIRIFLAGS="-Zmiri-preemption-rate=0.1" cargo +nightly miri test
//! ```

use ringmpsc_verified::spsc::{channel, fifo_holds};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

/// Fewer items under Miri, which interprets every step.
const ITEMS: u64 = if cfg!(miri) { 300 } else { 100_000 };

#[test]
fn two_threads_preserve_order() {
    let (mut producer, mut consumer) = channel::<u64>(3); // capacity 8: the producer must wait on the consumer

    let sender = thread::spawn(move || {
        for i in 0..ITEMS {
            let mut item = i;
            loop {
                match producer.push(item) {
                    Ok(()) => break,
                    Err(back) => {
                        item = back;
                        thread::yield_now();
                    }
                }
            }
        }
        producer
    });

    let receiver = thread::spawn(move || {
        let mut expect = 0u64;
        while expect < ITEMS {
            match consumer.pop() {
                Some(v) => {
                    assert_eq!(v, expect, "items arrived out of order");
                    expect += 1;
                }
                None => thread::yield_now(),
            }
        }
        consumer
    });

    let producer = sender.join().unwrap();
    let consumer = receiver.join().unwrap();
    // Erased at runtime; in the proof, this is where FIFO across threads is
    // stated for the two halves.
    fifo_holds(&producer, &consumer);
}

#[test]
fn batches_across_threads() {
    let (mut producer, mut consumer) = channel::<Box<u64>>(2); // capacity 4
    let n: u64 = if cfg!(miri) { 100 } else { 20_000 };

    let sender = thread::spawn(move || {
        for i in 0..n {
            let mut item = Box::new(i);
            while let Err(back) = producer.push(item) {
                item = back;
                thread::yield_now();
            }
        }
    });

    let mut got = 0u64;
    while got < n {
        let batch = consumer.consume_up_to(3);
        if batch.is_empty() {
            thread::yield_now();
        }
        for b in batch {
            assert_eq!(*b, got);
            got += 1;
        }
    }
    sender.join().unwrap();
}

/// Counts drops, to check that items left in the ring are dropped with it.
struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn dropping_the_consumer_drops_remaining_items() {
    let drops = Arc::new(AtomicUsize::new(0));
    let (mut producer, consumer) = channel::<Counted>(2);
    for _ in 0..3 {
        assert!(producer.push(Counted(drops.clone())).is_ok());
    }
    drop(consumer);
    assert_eq!(drops.load(Ordering::SeqCst), 3);
    drop(producer);
    assert_eq!(drops.load(Ordering::SeqCst), 3, "no item is dropped twice");
}

#[test]
fn dropping_the_single_threaded_ring_drops_remaining_items() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut ring = ringmpsc_verified::ring::Ring::<Counted>::new(2);
    for _ in 0..4 {
        assert!(ring.push(Counted(drops.clone())).is_ok());
    }
    let first = ring.pop();
    drop(first);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(ring);
    assert_eq!(drops.load(Ordering::SeqCst), 4);
}
