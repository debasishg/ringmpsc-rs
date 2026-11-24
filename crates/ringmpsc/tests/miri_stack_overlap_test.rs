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
//! Needs `--features stack-ring` (`required-features` in Cargo.toml).
//! Run with:
//!
//! ```text
//! MIRIFLAGS="-Zmiri-preemption-rate=0.1" \
//!   cargo +nightly miri test -p ringmpsc-rs --features stack-ring --test miri_stack_overlap_test
//!
//! MIRIFLAGS="-Zmiri-tree-borrows -Zmiri-preemption-rate=0.1" \
//!   cargo +nightly miri test -p ringmpsc-rs --features stack-ring --test miri_stack_overlap_test
//! ```
//!
//! Retry loops call `thread::yield_now()` rather than a backoff: these are
//! test loops, and yielding is what lets Miri switch threads.

use ringmpsc_rs::{StackChannel, StackRing};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

/// Items each producer pushes: three times the capacity, so every ring
/// fills and wraps several times.
const ITEMS: u64 = 24;
/// Producer batch size. Not a divisor of the capacity, so some reservations
/// are cut short at the end of the buffer.
const BATCH: usize = 3;

/// Counts producers that have finished, including by panicking, so a
/// consumer never waits forever for items a dead producer won't send.
struct FinishedOnDrop<'a>(&'a AtomicUsize);

impl Drop for FinishedOnDrop<'_> {
    fn drop(&mut self) {
        // Release: everything the producer committed happens-before a
        // consumer that observes the count.
        self.0.fetch_add(1, Ordering::Release);
    }
}

/// Runs `poll` until it has collected `expected` items, or until every
/// producer has finished and a final poll finds nothing more. Returns what
/// was collected, so a short result fails the caller's assertion instead of
/// hanging.
fn collect_until_done(
    expected: usize,
    producers: usize,
    finished: &AtomicUsize,
    mut poll: impl FnMut(&mut Vec<u64>) -> usize,
) -> Vec<u64> {
    let mut seen = Vec::with_capacity(expected);
    while seen.len() < expected {
        // Read the count before polling: if all producers had finished, this
        // poll sees everything they committed.
        let all_finished = finished.load(Ordering::Acquire) == producers;
        if poll(&mut seen) == 0 {
            if all_finished {
                break; // nothing more is coming
            }
            thread::yield_now();
        }
    }
    seen
}

/// Raw `StackRing` API: the producer writes multi-slot reservations while the
/// consumer reads multi-slot `peek` windows, and FIFO order is preserved.
#[test]
fn stack_ring_reserve_and_peek_overlap() {
    let ring: StackRing<u64, 8> = StackRing::new();
    let finished = AtomicUsize::new(0);

    thread::scope(|s| {
        // Producer: the ring's only producer thread.
        s.spawn(|| {
            let _finished = FinishedOnDrop(&finished);
            let mut next = 0u64;
            while next < ITEMS {
                let want = BATCH.min((ITEMS - next) as usize);
                // Safety: this thread is the ring's only producer; the
                // returned window is the producer's free region (INV-RES-01).
                let reserved = unsafe { ring.reserve(want) };
                let Some((ptr, len)) = reserved else {
                    thread::yield_now(); // full: let the consumer drain
                    continue;
                };
                for i in 0..len {
                    // Safety: i < len, so the write stays inside the reserved
                    // window (INV-RES-01, INV-MEM-02); `write` does not drop
                    // the uninitialized slot.
                    unsafe { ptr.add(i).write(next + i as u64) };
                }
                // Safety: all `len` reserved slots were written above, so
                // committing publishes only initialized slots (INV-INIT-01).
                unsafe { ring.commit(len) };
                next += len as u64;
            }
        });

        // Consumer: the ring's only consumer thread.
        let consumer = s.spawn(|| {
            collect_until_done(ITEMS as usize, 1, &finished, |seen| {
                // Safety: this thread is the ring's only consumer; `peek`
                // returns only published slots (INV-INIT-01).
                let Some((ptr, len)) = (unsafe { ring.peek() }) else {
                    return 0;
                };
                // Safety: i < len, inside the published window (INV-INIT-01,
                // INV-MEM-02). `u64` is `Copy`, so reading moves nothing out.
                seen.extend((0..len).map(|i| unsafe { ptr.add(i).read() }));
                ring.advance(len);
                len
            })
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
    const PRODUCERS: u64 = 2;
    let channel: StackChannel<u64, 8, 2> = StackChannel::new();
    let finished = AtomicUsize::new(0);

    thread::scope(|s| {
        for producer_id in 0..PRODUCERS {
            let (channel, finished) = (&channel, &finished);
            s.spawn(move || {
                let _finished = FinishedOnDrop(finished);
                let producer = channel.register().expect("a producer slot is free");
                // Tag each item with its producer in the top bits.
                let items: Vec<u64> = (0..ITEMS).map(|i| (producer_id << 32) | i).collect();
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
            let expected = (ITEMS * PRODUCERS) as usize;
            collect_until_done(expected, PRODUCERS as usize, &finished, |seen| {
                consumer.consume_all(|v| seen.push(*v))
            })
        });

        let seen = consumer.join().expect("consumer thread panicked");
        for producer_id in 0..PRODUCERS {
            let own: Vec<u64> = seen
                .iter()
                .filter(|v| *v >> 32 == producer_id)
                .map(|v| v & 0xffff_ffff)
                .collect();
            assert_eq!(own, (0..ITEMS).collect::<Vec<_>>(), "producer {producer_id} FIFO");
        }
    });
}
