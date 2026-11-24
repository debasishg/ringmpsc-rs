//! Two-thread `StackRing` / `StackChannel` tests whose producer and consumer
//! are live at the *same time*, sized to be tractable under Miri.
//!
//! This is the `StackRing` counterpart of `miri_overlap_test.rs`. The
//! threaded tests in `stack_ring_tests.rs` push thousands of items and don't
//! finish under Miri in reasonable time. These push a few dozen through an
//! 8-slot ring, so the producer has to wait for the consumer and the two
//! overlap. They exercise:
//! - multi-slot `reserve` and `peek` windows used while the other side runs
//!   (the pointer-provenance bug fixed in PR #36)
//! - windows that wrap around the end of the buffer
//! - data races between producer writes and consumer reads
//!
//! Run with:
//!
//! ```text
//! MIRIFLAGS="-Zmiri-preemption-rate=0.1" \
//!   cargo +nightly miri test -p ringmpsc-rs --features stack-ring --test miri_stack_overlap_test
//!
//! MIRIFLAGS="-Zmiri-tree-borrows -Zmiri-preemption-rate=0.1" \
//!   cargo +nightly miri test -p ringmpsc-rs --features stack-ring --test miri_stack_overlap_test
//! ```

#![cfg(feature = "stack-ring")]

use ringmpsc_rs::{StackChannel, StackRing};
use std::thread;

/// Items pushed per test: three times the capacity, so the ring fills and
/// wraps several times.
const ITEMS: u64 = 24;
/// Producer batch size. Not a divisor of the capacity, so some reservations
/// are cut short at the end of the buffer.
const BATCH: usize = 3;

/// Raw `StackRing` API: the producer writes multi-slot reservations while the
/// consumer reads multi-slot `peek` windows, and FIFO order is preserved.
#[test]
fn stack_ring_reserve_and_peek_overlap() {
    let ring: StackRing<u64, 8> = StackRing::new();

    thread::scope(|s| {
        // Producer: the ring's only producer thread.
        s.spawn(|| {
            let mut next = 0u64;
            while next < ITEMS {
                let want = BATCH.min((ITEMS - next) as usize);
                // Safety: this thread is the ring's only producer; writes stay
                // within the returned `len` (INV-RES-01), and exactly `len`
                // slots are written before `commit(len)`.
                let reserved = unsafe { ring.reserve(want) };
                match reserved {
                    Some((ptr, len)) => {
                        for i in 0..len {
                            // Safety: i < len, inside the reservation.
                            unsafe { ptr.add(i).write(next + i as u64) };
                        }
                        // Safety: all `len` reserved slots were written above.
                        unsafe { ring.commit(len) };
                        next += len as u64;
                    }
                    None => thread::yield_now(), // full: let the consumer drain
                }
            }
        });

        // Consumer: the ring's only consumer thread.
        let consumer = s.spawn(|| {
            let mut seen = Vec::with_capacity(ITEMS as usize);
            while seen.len() < ITEMS as usize {
                // Safety: this thread is the ring's only consumer; `peek`
                // returns only published slots (INV-INIT-01), and the reads
                // stay within the returned `len`.
                match unsafe { ring.peek() } {
                    Some((ptr, len)) => {
                        // Safety: i < len, inside the published window. `u64`
                        // is `Copy`, so reading does not move anything out.
                        seen.extend((0..len).map(|i| unsafe { ptr.add(i).read() }));
                        ring.advance(len);
                    }
                    None => thread::yield_now(), // empty: let the producer run
                }
            }
            seen
        });

        let seen = consumer.join().expect("consumer thread panicked");
        assert_eq!(seen, (0..ITEMS).collect::<Vec<_>>(), "FIFO order");
    });
}

/// `StackChannel`: two producers `send` batches (multi-slot copies through the
/// reserve pointer) while the consumer drains with `consume_all`. Each
/// producer's items arrive in order (INV-CH-03).
#[test]
fn stack_channel_send_and_consume_overlap() {
    let channel: StackChannel<u64, 8, 2> = StackChannel::new();

    thread::scope(|s| {
        for producer_id in 0..2u64 {
            let channel = &channel;
            s.spawn(move || {
                let producer = channel.register().expect("a producer slot is free");
                // Tag each item with its producer in the top bits.
                let items: Vec<u64> = (0..ITEMS / 2).map(|i| (producer_id << 32) | i).collect();
                let mut sent = 0;
                while sent < items.len() {
                    let end = (sent + BATCH).min(items.len());
                    let n = producer.send(&items[sent..end]);
                    sent += n;
                    if n == 0 {
                        thread::yield_now(); // ring full: let the consumer drain
                    }
                }
            });
        }

        let consumer = s.spawn(|| {
            let mut consumer = channel.consumer().expect("first consumer");
            let mut seen = Vec::with_capacity(ITEMS as usize);
            while seen.len() < ITEMS as usize {
                if consumer.consume_all(|v| seen.push(*v)) == 0 {
                    thread::yield_now();
                }
            }
            seen
        });

        let seen = consumer.join().expect("consumer thread panicked");
        for producer_id in 0..2u64 {
            let own: Vec<u64> = seen
                .iter()
                .filter(|v| *v >> 32 == producer_id)
                .map(|v| v & 0xffff_ffff)
                .collect();
            assert_eq!(own, (0..ITEMS / 2).collect::<Vec<_>>(), "producer {producer_id} FIFO");
        }
    });
}
