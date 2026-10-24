//! Runtime checks of the verified ring, compiled as ordinary Rust (the ghost
//! code is erased). The proofs cover every input; these confirm the erased
//! build behaves the same, including under Miri:
//!
//! ```text
//! CARGO_TARGET_DIR=target/plain cargo test
//! CARGO_TARGET_DIR=target/miri cargo +nightly miri test
//! ```
//!
//! A separate target directory keeps these builds from making the next
//! `cargo verus` think there is nothing to verify.

use ringmpsc_verified::ring::Ring;
use std::collections::VecDeque;

#[test]
fn fifo_with_wrap_around() {
    let mut ring = Ring::<u64>::new(2); // capacity 4
    assert_eq!(ring.capacity(), 4);
    let mut next = 0u64;
    let mut expect = 0u64;
    // Many laps around a 4-slot ring.
    for _ in 0..50 {
        for _ in 0..3 {
            assert!(ring.push(next).is_ok());
            next += 1;
        }
        for _ in 0..3 {
            assert_eq!(ring.pop(), Some(expect));
            expect += 1;
        }
    }
    assert!(ring.is_empty());
    assert_eq!(ring.pop(), None);
}

#[test]
fn full_ring_hands_the_item_back() {
    let mut ring = Ring::<String>::new(1); // capacity 2
    assert!(ring.push("a".into()).is_ok());
    assert!(ring.push("b".into()).is_ok());
    assert_eq!(ring.push("c".into()), Err("c".to_string()));
    assert_eq!(ring.pop().as_deref(), Some("a"));
    assert!(ring.push("c".into()).is_ok());
    assert_eq!(ring.consume_up_to(10), vec!["b".to_string(), "c".to_string()]);
    assert!(ring.is_empty());
}

#[test]
fn consume_up_to_takes_the_oldest() {
    let mut ring = Ring::<Box<u32>>::new(3); // capacity 8
    for i in 0..6 {
        assert!(ring.push(Box::new(i)).is_ok());
    }
    let first: Vec<u32> = ring.consume_up_to(4).into_iter().map(|b| *b).collect();
    assert_eq!(first, vec![0, 1, 2, 3]);
    assert_eq!(ring.len(), 2);
    let rest: Vec<u32> = ring.consume_up_to(4).into_iter().map(|b| *b).collect();
    assert_eq!(rest, vec![4, 5]);
}

/// Random operations against a VecDeque model, with a small LCG so the test
/// needs no dependencies.
#[test]
fn matches_a_vecdeque_model() {
    let mut ring = Ring::<u64>::new(2);
    let mut model = VecDeque::new();
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = 0u64;
    for _ in 0..20_000 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        match (seed >> 61) % 3 {
            0 => {
                let pushed = ring.push(next).is_ok();
                assert_eq!(pushed, model.len() < 4);
                if pushed {
                    model.push_back(next);
                }
                next += 1;
            }
            1 => assert_eq!(ring.pop(), model.pop_front()),
            _ => {
                let k = ((seed >> 40) % 5) as usize;
                let got = ring.consume_up_to(k);
                let want: Vec<u64> = model.drain(..k.min(model.len())).collect();
                assert_eq!(got, want);
            }
        }
        assert_eq!(ring.len(), model.len());
    }
}
