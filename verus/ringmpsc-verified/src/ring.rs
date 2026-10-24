//! A verified single-threaded SPSC ring (phase 3, #22).
//!
//! The same design as `crates/ringmpsc/src/ring.rs`: unbounded `u64`
//! sequence numbers, slot `seq & mask`, a producer-side `cached_head` and a
//! consumer-side `cached_tail` with fast and slow paths. Each slot is a
//! `PCell<T>` (`UnsafeCell<MaybeUninit<T>>` underneath, the production
//! layout), and every access goes through a `PointsTo` token the
//! `SpscSlots` protocol hands out. That makes these facts proofs, not
//! comments:
//!
//! - **no uninitialized read**: `take` needs an initialized token, and the
//!   protocol hands one out only for a published sequence number
//! - **no double free**: a slot's token is unique, and taking an item leaves
//!   it uninitialized until the producer writes it again
//! - **FIFO**: `pop` returns exactly the oldest item `push` accepted
//!
//! Single-threaded: the ring holds every token itself. Phase 4 (#23) splits
//! the tokens between a producer half and a consumer half and puts `head`
//! and `tail` in atomics.

use vstd::cell::pcell_maybe_uninit::PCell;
use vstd::prelude::*;
use crate::arith::{capacity, slot_index};
use crate::slots::SpscSlots;
// Used only in specs and proofs, which plain builds erase.
#[cfg(verus_only)]
use crate::arith::{cap_spec, MAX_RING_BITS};
#[cfg(verus_only)]
use vstd::cell::pcell_maybe_uninit::PointsTo;

verus! {

/// Every protocol token, held by the single-threaded ring.
pub tracked struct Tokens<T> {
    pub inst: SpscSlots::Instance<T>,
    pub head: SpscSlots::head<T>,
    pub tail: SpscSlots::tail<T>,
    pub cached_head: SpscSlots::cached_head<T>,
    pub cached_tail: SpscSlots::cached_tail<T>,
    pub producing: SpscSlots::producing<T>,
    pub consuming: SpscSlots::consuming<T>,
    pub produced: SpscSlots::produced<T>,
    pub consumed: SpscSlots::consumed<T>,
}

pub struct Ring<T> {
    cells: Vec<PCell<T>>,
    ring_bits: u8,
    cap: usize,
    head: u64,
    tail: u64,
    cached_head: u64,
    cached_tail: u64,
    // Ghost: erased in plain builds, where nothing reads it.
    #[allow(dead_code)]
    toks: Tracked<Tokens<T>>,
}

impl<T> Ring<T> {
    /// The ring's contents, oldest first: what was produced and not yet
    /// consumed.
    pub closed spec fn view(&self) -> Seq<T> {
        self.toks@.produced.value().skip(self.toks@.consumed.value().len() as int)
    }

    pub closed spec fn capacity_spec(&self) -> nat {
        self.cap as nat
    }

    /// All `u64` sequence numbers used up: at 10 billion items a second,
    /// after about 58 years. The ring then refuses further pushes instead of
    /// wrapping (INV-SEQ-03 as a checked fact).
    pub closed spec fn exhausted(&self) -> bool {
        self.tail == u64::MAX
    }

    pub closed spec fn wf(&self) -> bool {
        let t = self.toks@;
        &&& 1 <= self.ring_bits <= MAX_RING_BITS
        &&& self.cap as int == cap_spec(self.ring_bits)
        &&& t.inst.cap() == self.cap as nat
        &&& self.cells@.len() == self.cap as nat
        &&& forall |i: int| 0 <= i < self.cap ==> #[trigger] self.cells@[i].id() == t.inst.cells()[i]
        &&& t.head.instance_id() == t.inst.id()
        &&& t.tail.instance_id() == t.inst.id()
        &&& t.cached_head.instance_id() == t.inst.id()
        &&& t.cached_tail.instance_id() == t.inst.id()
        &&& t.producing.instance_id() == t.inst.id()
        &&& t.consuming.instance_id() == t.inst.id()
        &&& t.produced.instance_id() == t.inst.id()
        &&& t.consumed.instance_id() == t.inst.id()
        &&& t.head.value() == self.head as nat
        &&& t.tail.value() == self.tail as nat
        &&& t.cached_head.value() == self.cached_head as nat
        &&& t.cached_tail.value() == self.cached_tail as nat
        &&& !t.producing.value()
        &&& !t.consuming.value()
    }

    /// Learn the protocol's cursor invariants from the tokens.
    proof fn lemma_cursors(tracked &self)
        requires
            self.wf(),
        ensures
            self.head <= self.tail,
            self.tail - self.head <= self.cap,
            self.cached_head <= self.head,
            self.cached_tail <= self.tail,
            self.toks@.produced.value().len() == self.tail,
            self.toks@.consumed.value().len() == self.head,
            crate::slots::is_prefix(self.toks@.consumed.value(), self.toks@.produced.value()),
            self@.len() == self.tail - self.head,
    {
        let tracked t = self.toks.borrow();
        t.inst.cursor_facts(&t.head, &t.tail, &t.cached_head, &t.cached_tail, &t.consuming, &t.produced, &t.consumed);
    }

    /// An empty ring with `2^ring_bits` slots (`Config::new`'s range).
    pub fn new(ring_bits: u8) -> (r: Self)
        requires
            1 <= ring_bits <= MAX_RING_BITS,
        ensures
            r.wf(),
            r@ == Seq::<T>::empty(),
            r.capacity_spec() == cap_spec(ring_bits),
            !r.exhausted(),
    {
        let cap = capacity(ring_bits);
        let mut cells: Vec<PCell<T>> = Vec::new();
        let tracked mut perms: Map<nat, PointsTo<T>> = Map::tracked_empty();
        let mut i: usize = 0;
        while i < cap
            invariant
                i <= cap,
                cells@.len() == i,
                forall |s: nat| #[trigger] perms.dom().contains(s) <==> s < i,
                forall |s: nat| s < i ==> {
                    &&& #[trigger] perms[s].id() == cells@[s as int].id()
                    &&& perms[s].is_uninit()
                },
            decreases cap - i,
        {
            let (cell, Tracked(perm)) = PCell::empty();
            cells.push(cell);
            proof {
                perms.tracked_insert(i as nat, perm);
            }
            i = i + 1;
        }
        let ghost ids = cells@.map_values(|c: PCell<T>| c.id());
        let tracked (
            Tracked(inst),
            Tracked(head),
            Tracked(tail),
            Tracked(cached_head),
            Tracked(cached_tail),
            Tracked(producing),
            Tracked(consuming),
            Tracked(produced),
            Tracked(consumed),
        ) = SpscSlots::Instance::initialize(cap as nat, ids, perms, perms);
        Ring {
            cells,
            ring_bits,
            cap,
            head: 0,
            tail: 0,
            cached_head: 0,
            cached_tail: 0,
            toks: Tracked(Tokens { inst, head, tail, cached_head, cached_tail, producing, consuming, produced, consumed }),
        }
    }

    pub fn capacity(&self) -> (n: usize)
        requires
            self.wf(),
        ensures
            n == self.capacity_spec(),
    {
        self.cap
    }

    /// `Ring::len`: `tail - head`.
    pub fn len(&self) -> (n: usize)
        requires
            self.wf(),
        ensures
            n == self@.len(),
    {
        proof { self.lemma_cursors(); }
        (self.tail - self.head) as usize
    }

    pub fn is_empty(&self) -> (b: bool)
        requires
            self.wf(),
        ensures
            b == (self@.len() == 0),
    {
        proof { self.lemma_cursors(); }
        self.tail == self.head
    }

    /// `Ring::push`: append `v`, or hand it back if the ring is full.
    ///
    /// Mirrors `reserve_shared`: check space against `cached_head` first,
    /// and reload `head` only if the cache says the ring is full.
    pub fn push(&mut self, v: T) -> (r: Result<(), T>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            final(self).capacity_spec() == old(self).capacity_spec(),
            r is Ok ==> final(self)@ == old(self)@.push(v),
            r is Err ==> final(self)@ == old(self)@ && r->Err_0 == v,
            old(self)@.len() < old(self).capacity_spec() && !old(self).exhausted() ==> r is Ok,
    {
        proof { self.lemma_cursors(); }
        if self.tail == u64::MAX {
            return Err(v);
        }
        // Fast path: space by the cached head. Slow path: refresh it.
        if self.tail - self.cached_head >= self.cap as u64 {
            self.cached_head = self.head;
            proof {
                let tracked t = self.toks.borrow_mut();
                t.inst.refresh_head(&t.head, &mut t.cached_head);
            }
            if self.tail - self.cached_head >= self.cap as u64 {
                return Err(v);
            }
        }
        let idx = slot_index(self.tail, self.ring_bits);
        let tracked mut perm: PointsTo<T>;
        proof {
            let tracked t = self.toks.borrow_mut();
            perm = t.inst.produce_start(&t.cached_head, &mut t.producing, &t.produced);
        }
        self.cells[idx].put(Tracked(&mut perm), v);
        proof {
            let tracked t = self.toks.borrow_mut();
            t.inst.produce_end(perm, perm, &mut t.tail, &mut t.producing, &mut t.produced);
        }
        self.tail = self.tail + 1;
        proof {
            assert(self@ =~= old(self)@.push(v));
        }
        Ok(())
    }

    /// Remove and return the oldest item, if any.
    ///
    /// Mirrors `readable`'s fixed fast path (#17): read by the cached tail
    /// only when `cached_tail > head`, otherwise reload `tail`.
    pub fn pop(&mut self) -> (r: Option<T>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            final(self).capacity_spec() == old(self).capacity_spec(),
            final(self).exhausted() == old(self).exhausted(),
            old(self)@.len() == 0 ==> r is None && final(self)@ == old(self)@,
            old(self)@.len() > 0 ==> r == Some(old(self)@[0]) && final(self)@ == old(self)@.drop_first(),
    {
        proof { self.lemma_cursors(); }
        if self.cached_tail <= self.head {
            self.cached_tail = self.tail;
            proof {
                let tracked t = self.toks.borrow_mut();
                t.inst.refresh_tail(&t.tail, &mut t.cached_tail);
            }
            if self.cached_tail <= self.head {
                return None;
            }
        }
        let idx = slot_index(self.head, self.ring_bits);
        let tracked mut perm: PointsTo<T>;
        proof {
            let tracked t = self.toks.borrow_mut();
            perm = t.inst.consume_start(&t.cached_tail, &mut t.consuming, &mut t.consumed);
        }
        let v = self.cells[idx].take(Tracked(&mut perm));
        proof {
            let tracked t = self.toks.borrow_mut();
            t.inst.consume_end(perm, perm, &mut t.head, &mut t.consuming);
        }
        self.head = self.head + 1;
        proof {
            // The item taken is the oldest one: FIFO says the consumer's new
            // history entry equals the produced item at the old head.
            self.lemma_cursors();
            assert(self.toks@.consumed.value()[old(self).head as int] == self.toks@.produced.value()[old(self).head as int]);
            assert(self@ =~= old(self)@.drop_first());
        }
        Some(v)
    }

    /// `consume_up_to_owned`: remove up to `max` of the oldest items, in order.
    pub fn consume_up_to(&mut self, max: usize) -> (out: Vec<T>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            final(self).capacity_spec() == old(self).capacity_spec(),
            final(self).exhausted() == old(self).exhausted(),
            out@.len() == if max < old(self)@.len() { max as int } else { old(self)@.len() as int },
            out@ == old(self)@.take(out@.len() as int),
            final(self)@ == old(self)@.skip(out@.len() as int),
    {
        let mut out: Vec<T> = Vec::new();
        while out.len() < max && !self.is_empty()
            invariant
                self.wf(),
                self.capacity_spec() == old(self).capacity_spec(),
                self.exhausted() == old(self).exhausted(),
                out@.len() <= max,
                old(self)@ == out@ + self@,
            decreases max - out.len(),
        {
            let v = self.pop();
            proof {
                assert(self@.len() + 1 == old(self)@.len() - out@.len());
            }
            match v {
                Some(item) => {
                    out.push(item);
                },
                None => {
                    // Unreachable: the loop runs only while the ring is
                    // non-empty, and then `pop` returns `Some`.
                    proof { assert(false); }
                },
            }
        }
        proof {
            assert(out@ =~= old(self)@.take(out@.len() as int));
            assert(self@ =~= old(self)@.skip(out@.len() as int));
        }
        out
    }
}

} // verus!

/// Dropping the ring drops the items still in it, as the production ring's
/// `Drop` does. Without this they would leak (safely: a `PCell` never drops
/// its contents).
///
/// Trusted glue, outside `verus!`: it calls the verified `consume_up_to`,
/// whose precondition `wf()` holds for every `Ring` safe code can obtain.
/// The fields are private, `new` establishes `wf()`, and every method
/// preserves it.
impl<T> Drop for Ring<T> {
    fn drop(&mut self) {
        let n = self.len();
        let _ = self.consume_up_to(n);
    }
}
