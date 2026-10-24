//! Differential property tests: the verified ring against the production
//! ring (phase 7, #26).
//!
//! Each case is a random sequence of producer and consumer operations,
//! applied to the production `Ring<u64>` (`crates/ringmpsc`), to the verified
//! `spsc` halves, and to a `VecDeque` model. After every operation the three
//! must agree on what was accepted and what came out.
//!
//! The rings differ in one deliberate way: production reservations and
//! `readable()` stop at the buffer's wrap point (they hand out contiguous
//! slices), and the verified ring's don't. Where that matters, the
//! production result drives the verified ring: it reserves what production
//! granted, and reads as many items as production's `readable()` returned.
//! Both must then behave identically.
//!
//! ```text
//! CARGO_TARGET_DIR=target/plain cargo test --test differential
//! PROPTEST_CASES=5000 ...   # a deeper run
//! ```


// Proptest's case volume is too slow under Miri; this runs natively only.
#![cfg(not(miri))]
use proptest::prelude::*;
use ringmpsc_rs::{Config, Ring};
use ringmpsc_verified::spsc::{channel, Consumer, Producer};
use std::collections::VecDeque;

/// `ring_bits = 2`: four slots, so wrap-around happens constantly.
const RING_BITS: u8 = 2;
const CAPACITY: usize = 4;

#[derive(Debug, Clone)]
enum Op {
    /// `push(v)`.
    Push,
    /// `reserve(n)`, write `w` items, `commit_up_to(k)`.
    Batch { n: usize, w: usize, k: usize },
    /// `reserve(n)`, write `w` items, abandon (drop without committing).
    Abandon { n: usize, w: usize },
    /// `consume_up_to(k)`, owned.
    ConsumeUpTo(usize),
    /// `readable()`, read the first `k` items in place, `advance(k)`.
    ReadAdvance(usize),
    /// `recv(out[..k])`.
    Recv(usize),
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => Just(Op::Push),
        3 => (1usize..6, 0usize..6, 0usize..6).prop_map(|(n, w, k)| Op::Batch { n, w, k }),
        1 => (1usize..6, 0usize..6).prop_map(|(n, w)| Op::Abandon { n, w }),
        2 => (0usize..6).prop_map(Op::ConsumeUpTo),
        2 => (0usize..6).prop_map(Op::ReadAdvance),
        1 => (0usize..6).prop_map(Op::Recv),
    ]
}

/// The three implementations side by side.
struct Harness {
    prod: Ring<u64>,
    producer: Producer<u64>,
    consumer: Consumer<u64>,
    model: VecDeque<u64>,
    next: u64,
}

impl Harness {
    fn new() -> Self {
        let (producer, consumer) = channel::<u64>(RING_BITS);
        Self {
            prod: Ring::new(Config::new(RING_BITS, 1, false)),
            producer,
            consumer,
            model: VecDeque::new(),
            next: 0,
        }
    }

    fn fresh(&mut self, count: usize) -> Vec<u64> {
        let v: Vec<u64> = (self.next..self.next + count as u64).collect();
        self.next += count as u64;
        v
    }

    fn apply(&mut self, op: &Op) -> Result<(), TestCaseError> {
        match *op {
            Op::Push => {
                let v = self.fresh(1)[0];
                let prod_ok = self.prod.push(v);
                let ver_ok = self.producer.push(v).is_ok();
                prop_assert_eq!(prod_ok, ver_ok, "push accepted differently");
                prop_assert_eq!(prod_ok, self.model.len() < CAPACITY);
                if prod_ok {
                    self.model.push_back(v);
                }
            }
            Op::Batch { n, w, k } => {
                let (granted, committed) = match self.prod.reserve(n) {
                    None => (0, 0),
                    Some(mut r) => {
                        let granted = r.len();
                        // Touch only `self.next`: `self.prod` is borrowed by `r`.
                        let count = w.min(granted) as u64;
                        let items: Vec<u64> = (self.next..self.next + count).collect();
                        self.next += count;
                        r.extend(items.iter().copied());
                        let committed = r.commit_up_to(k);
                        // Same reservation on the verified ring.
                        let ver_granted = self.producer.reserve(granted);
                        prop_assert_eq!(ver_granted, granted, "verified ring granted a different size");
                        for &v in &items {
                            self.producer.write_next(v);
                        }
                        let ver_committed = self.producer.commit_up_to(k);
                        prop_assert_eq!(ver_committed, committed, "committed counts differ");
                        self.model.extend(items[..committed].iter().copied());
                        (granted, committed)
                    }
                };
                prop_assert!(committed <= granted && granted <= n);
                prop_assert!(self.model.len() <= CAPACITY);
            }
            Op::Abandon { n, w } => {
                if let Some(mut r) = self.prod.reserve(n) {
                    let granted = r.len();
                    let count = w.min(granted) as u64;
                    let items: Vec<u64> = (self.next..self.next + count).collect();
                    self.next += count;
                    r.extend(items.iter().copied());
                    drop(r); // dropped without committing: nothing published
                    prop_assert_eq!(self.producer.reserve(granted), granted);
                    for &v in &items {
                        self.producer.write_next(v);
                    }
                    self.producer.abandon();
                }
            }
            Op::ConsumeUpTo(k) => {
                let mut prod_out = Vec::new();
                self.prod.consume_up_to_owned(k, |v| prod_out.push(v));
                let ver_out = self.consumer.consume_up_to(k);
                let model_out: Vec<u64> = self.model.drain(..k.min(self.model.len())).collect();
                prop_assert_eq!(&prod_out, &model_out, "production consume_up_to");
                prop_assert_eq!(&ver_out, &model_out, "verified consume_up_to");
            }
            Op::ReadAdvance(k) => {
                // Both `readable`s use their consumer's cached tail, so each
                // may hold fewer than everything published (but at least one
                // item when anything is). Production's also stops at the wrap
                // point. Read and consume what both hold.
                let prod_seen: Vec<u64> = self.prod.readable().map(<[u64]>::to_vec).unwrap_or_default();
                let held = self.consumer.readable();
                prop_assert!(held <= self.model.len());
                prop_assert_eq!(held == 0, self.model.is_empty(), "verified holds something iff something is published");
                prop_assert!(prod_seen.len() <= self.model.len());
                let n = k.min(prod_seen.len()).min(held);
                for (i, expect) in prod_seen.iter().enumerate().take(n) {
                    prop_assert_eq!(self.consumer.get(i), expect, "in-place read differs");
                    prop_assert_eq!(Some(expect), self.model.get(i));
                }
                if n > 0 {
                    self.prod.advance(n);
                }
                if held > 0 {
                    // Consume the same `n` on the verified ring; the rest go back.
                    self.consumer.advance(n);
                }
                self.model.drain(..n);
            }
            Op::Recv(k) => {
                let mut prod_buf = vec![0u64; k];
                let got = self.prod.recv(&mut prod_buf);
                // `recv` also reads through the cached tail, so the verified
                // ring may return fewer at once; keep receiving (each call
                // makes progress while items are published) to stay in step.
                let mut ver_buf = Vec::new();
                let mut ver_got = 0;
                while ver_got < got {
                    let n = self.consumer.recv(&mut ver_buf, got - ver_got);
                    prop_assert!(n > 0, "verified recv made no progress");
                    ver_got += n;
                }
                prop_assert_eq!(&ver_buf[..], &prod_buf[..got]);
                let model_out: Vec<u64> = self.model.drain(..got).collect();
                prop_assert_eq!(&prod_buf[..got], &model_out[..]);
            }
        }
        Ok(())
    }

    /// Drain both rings and compare with the model.
    fn finish(&mut self) -> Result<(), TestCaseError> {
        let mut prod_rest = Vec::new();
        self.prod.consume_up_to_owned(usize::MAX, |v| prod_rest.push(v));
        let ver_rest = self.consumer.consume_up_to(usize::MAX);
        let model_rest: Vec<u64> = self.model.drain(..).collect();
        prop_assert_eq!(&prod_rest, &model_rest);
        prop_assert_eq!(&ver_rest, &model_rest);
        Ok(())
    }
}

proptest! {
    /// Production, verified and model agree after every operation.
    #[test]
    fn verified_matches_production_and_model(
        ops in proptest::collection::vec(op_strategy(), 1..300),
    ) {
        let mut h = Harness::new();
        for op in &ops {
            h.apply(op)?;
        }
        h.finish()?;
    }
}

proptest! {
    /// The verified ring alone against the model, with its own (wider)
    /// reservations and reads: no production ring constraining the sizes.
    #[test]
    fn verified_alone_matches_model(
        ops in proptest::collection::vec(op_strategy(), 1..300),
    ) {
        let (mut p, mut c) = channel::<u64>(RING_BITS);
        let mut model: VecDeque<u64> = VecDeque::new();
        let mut next = 0u64;
        for op in &ops {
            match *op {
                Op::Push => {
                    let ok = p.push(next).is_ok();
                    prop_assert_eq!(ok, model.len() < CAPACITY);
                    if ok { model.push_back(next); }
                    next += 1;
                }
                Op::Batch { n, w, k } => {
                    // All or nothing, as production's `reserve` (which can
                    // also be shortened by a wrap; this ring can't).
                    let granted = p.reserve(n);
                    prop_assert_eq!(granted, if n <= CAPACITY - model.len() { n } else { 0 });
                    if granted > 0 {
                        let items: Vec<u64> = (next..next + w.min(granted) as u64).collect();
                        next += items.len() as u64;
                        for &v in &items { p.write_next(v); }
                        let committed = p.commit_up_to(k);
                        prop_assert_eq!(committed, k.min(items.len()));
                        model.extend(items[..committed].iter().copied());
                    }
                }
                Op::Abandon { n, w } => {
                    let granted = p.reserve(n);
                    if granted > 0 {
                        for i in 0..w.min(granted) { p.write_next(next + i as u64); }
                        next += w.min(granted) as u64;
                        p.abandon();
                    }
                }
                Op::ConsumeUpTo(k) => {
                    let out = c.consume_up_to(k);
                    let want: Vec<u64> = model.drain(..k.min(model.len())).collect();
                    prop_assert_eq!(out, want);
                }
                Op::ReadAdvance(k) => {
                    // Cached path: at most everything published, and at least
                    // one item when anything is.
                    let held = c.readable();
                    prop_assert!(held <= model.len());
                    prop_assert_eq!(held == 0, model.is_empty());
                    let n = k.min(held);
                    for i in 0..n { prop_assert_eq!(*c.get(i), model[i]); }
                    if held > 0 { c.advance(n); }
                    model.drain(..n);
                }
                Op::Recv(k) => {
                    let mut out = Vec::new();
                    let got = c.recv(&mut out, k);
                    prop_assert!(got <= k.min(model.len()));
                    prop_assert_eq!(got == 0, k == 0 || model.is_empty());
                    let want: Vec<u64> = model.drain(..got).collect();
                    prop_assert_eq!(out, want);
                }
            }
        }
    }
}
