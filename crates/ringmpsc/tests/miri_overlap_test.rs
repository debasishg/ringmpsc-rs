//! Minimal two-thread test whose producer and consumer are live at the *same
//! time*, sized to be tractable under Miri.
//!
//! The existing concurrent tests don't serve this purpose: `test_concurrent_stress`
//! overlaps but pushes 400k items, and `test_fifo_ordering_multi_producer` joins
//! every producer before it consumes, so nothing ever overlaps.
//!
//! Run with:
//!
//! ```text
//! MIRIFLAGS="-Zmiri-preemption-rate=0.1" \
//!   cargo +nightly miri test --test miri_overlap_test
//!
//! MIRIFLAGS="-Zmiri-tree-borrows -Zmiri-preemption-rate=0.1" \
//!   cargo +nightly miri test --test miri_overlap_test
//! ```

use ringmpsc_rs::{Channel, Config};
use std::sync::Arc;
use std::thread;

#[test]
fn spsc_producer_and_consumer_overlap() {
    const ITEMS: u64 = 24;

    // 8 slots against 24 items: the producer must block and wait for the
    // consumer to drain, which is what forces the two to be live together.
    let config = Config::new(3, 1, false);
    let channel = Arc::new(Channel::<u64>::new(config));

    let ch = Arc::clone(&channel);
    let producer = thread::spawn(move || {
        let p = ch.register().unwrap();
        for i in 0..ITEMS {
            while !p.push(i) {
                thread::yield_now();
            }
        }
    });

    let ch = Arc::clone(&channel);
    let consumer = thread::spawn(move || {
        let mut total = 0usize;
        let mut sum = 0u64;
        while total < ITEMS as usize {
            total += ch.consume_all(|item| sum += item);
            if total < ITEMS as usize {
                thread::yield_now();
            }
        }
        sum
    });

    producer.join().unwrap();
    let sum = consumer.join().unwrap();
    assert_eq!(sum, (0..ITEMS).sum::<u64>());
}
