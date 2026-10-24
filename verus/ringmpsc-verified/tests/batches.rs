//! Runtime checks of the verified batch API (phase 5, #24), compiled as
//! ordinary Rust (the ghost code is erased), natively and under Miri:
//!
//! ```text
//! CARGO_TARGET_DIR=target/plain cargo test
//! CARGO_TARGET_DIR=target/miri MIRIFLAGS="-Zmiri-preemption-rate=0.1" cargo +nightly miri test
//! ```

use ringmpsc_verified::spsc::channel;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

/// Counts drops, to check what the ring drops and when.
struct Counted(u64, Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.1.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn reserve_write_commit_in_order() {
    let (mut p, mut c) = channel::<u64>(3); // capacity 8
    let got = p.reserve(5);
    assert_eq!(got, 5);
    for v in 10..15 {
        p.write_next(v);
    }
    assert_eq!(p.commit(), 5);
    assert_eq!(c.consume_up_to(10), vec![10, 11, 12, 13, 14]);
}

#[test]
fn reserve_is_capped_by_free_space() {
    let (mut p, mut c) = channel::<u64>(2); // capacity 4
    assert_eq!(p.reserve(10), 4);
    for v in 0..4 {
        p.write_next(v);
    }
    assert_eq!(p.commit(), 4);
    assert_eq!(p.reserve(1), 0, "the ring is full");
    assert_eq!(c.pop(), Some(0));
    assert_eq!(p.reserve(3), 1, "one slot was freed");
    p.abandon();
}

#[test]
fn partial_commit_publishes_a_prefix_and_drops_the_rest() {
    let drops = Arc::new(AtomicUsize::new(0));
    let (mut p, mut c) = channel::<Counted>(3);
    assert_eq!(p.reserve(4), 4);
    for v in 0..3 {
        p.write_next(Counted(v, drops.clone()));
    }
    // Three written, commit two: the third is dropped, the fourth was never written.
    assert_eq!(p.commit_up_to(2), 2);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let out = c.consume_up_to(10);
    assert_eq!(out.iter().map(|x| x.0).collect::<Vec<_>>(), vec![0, 1]);
    drop(out);
    assert_eq!(drops.load(Ordering::SeqCst), 3);
}

#[test]
fn abandon_drops_what_was_written() {
    let drops = Arc::new(AtomicUsize::new(0));
    let (mut p, mut c) = channel::<Counted>(2);
    assert_eq!(p.reserve(3), 3);
    p.write_next(Counted(1, drops.clone()));
    p.write_next(Counted(2, drops.clone()));
    p.abandon();
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert!(c.pop().is_none(), "nothing was published");
}

#[test]
fn readable_get_release() {
    let (mut p, mut c) = channel::<String>(3);
    for s in ["a", "b", "c", "d"] {
        assert!(p.push(s.to_string()).is_ok());
    }
    assert_eq!(c.readable(), 4);
    // Zero-copy reads, in place.
    assert_eq!(c.get(0), "a");
    assert_eq!(c.get(3), "d");
    // Consume two; the other two go back to the ring.
    assert_eq!(c.release(2), vec!["a".to_string(), "b".to_string()]);
    assert_eq!(c.readable(), 2);
    assert_eq!(c.get(0), "c");
    c.advance(2);
    assert_eq!(c.readable(), 0);
}

#[test]
fn recv_copies_out() {
    let (mut p, mut c) = channel::<u32>(2);
    for v in [7, 8, 9] {
        assert!(p.push(v).is_ok());
    }
    let mut out = vec![1];
    assert_eq!(c.recv(&mut out, 2), 2);
    assert_eq!(out, vec![1, 7, 8]);
    assert_eq!(c.recv(&mut out, 5), 1);
    assert_eq!(out, vec![1, 7, 8, 9]);
    assert_eq!(c.recv(&mut out, 5), 0);
}

/// Random batch sizes on both sides, across two threads.
#[test]
fn batches_across_threads() {
    let n: u64 = if cfg!(miri) { 120 } else { 50_000 };
    let (mut p, mut c) = channel::<u64>(3);
    let sender = thread::spawn(move || {
        let mut next = 0u64;
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        while next < n {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let want = 1 + ((seed >> 33) % 5) as usize;
            let got = p.reserve(want);
            if got == 0 {
                thread::yield_now();
                continue;
            }
            let mut written = 0;
            while written < got && next + (written as u64) < n {
                p.write_next(next + written as u64);
                written += 1;
            }
            // Sometimes publish fewer than written; the rest is dropped and resent.
            let k = if (seed >> 20) % 3 == 0 && written > 1 { written - 1 } else { written };
            next += p.commit_up_to(k) as u64;
        }
    });
    let mut expect = 0u64;
    while expect < n {
        let held = c.readable();
        if held == 0 {
            thread::yield_now();
            continue;
        }
        let k = 1 + (expect as usize % held);
        for i in 0..k {
            assert_eq!(*c.get(i), expect + i as u64);
        }
        c.advance(k);
        expect += k as u64;
    }
    sender.join().unwrap();
}
