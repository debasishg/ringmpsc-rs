//! A verified concurrent SPSC ring (phases 4 and 5, #23, #24).
//!
//! `channel(ring_bits)` returns a `Producer` and a `Consumer` that can run
//! on different threads. They share the cells and two atomics through an
//! `Arc`:
//!
//! - `tail`, a `vstd` `AtomicU64` whose invariant holds the protocol's
//!   `tail` token: the published tail always equals the token's value
//! - `head`, likewise for the `head` token
//!
//! Each half owns the rest of its side's tokens: the producer its
//! `cached_head`, `reserved` count and `produced` history, the consumer its
//! `cached_tail`, `held` count and values, and `consumed` history. Neither
//! is `Clone`, and every method takes `&mut self`, so there is exactly one
//! of each, the property #12 enforces in the production crate.
//!
//! Every protocol step that touches a shared cursor happens inside an
//! atomic operation on it (`atomic_with_ghost!`), the same points where the
//! production code does its `load`s and `store`s. So the proofs hold under
//! any interleaving of the two threads.
//!
//! **Batches (#24).** The producer reserves `n` slots (`reserve`), fills
//! them in order (`write_next`), and publishes the first `k` filled ones
//! (`commit_up_to`); unpublished writes are dropped. The consumer holds every
//! published slot (`readable`), borrows any of them in place (`get`, no
//! copy), and consumes the first `k` (`release`, `advance`, `recv`). `push`
//! and `pop` are batches of one.
//!
//! The producer writes through `write_next`, not through a raw
//! `&mut [MaybeUninit<T>]` slice: with a slice, nothing could show which
//! slots were written, and committing an unwritten slot is undefined
//! behavior. The production `Reservation::commit` had exactly that hole
//! (#32), and now counts writes the same way.
//!
//! **Orderings:** `vstd` atomics are sequentially consistent, and Verus
//! reasons about them that way. The proof shows the protocol is race-free.
//! It does not show that the production code's `Relaxed`/`Acquire`/`Release`
//! are enough; that stays with loom and Miri.

use std::sync::Arc;
use vstd::atomic_ghost::*;
use vstd::cell::pcell_maybe_uninit::{PCell, PointsTo};
use vstd::prelude::*;
use crate::arith::{capacity, slot_index};
use crate::slots::SpscSlots;
#[cfg(verus_only)]
use crate::arith::{cap_spec, MAX_RING_BITS};
#[cfg(verus_only)]
use crate::slots::{is_prefix, range, release_keys};

verus! {

/// `Arc::clone` points at the same value. `vstd` has no spec for it, so
/// this one-line wrapper states it.
#[verifier::external_body]
pub(crate) fn arc_clone<S>(a: &Arc<S>) -> (r: Arc<S>)
    ensures
        *r == **a,
{
    Arc::clone(a)
}

/// The `head` atomic's invariant: it holds the protocol's `head` token, and
/// the token's value is the atomic's value.
pub struct HeadInv;

impl<T> AtomicInvariantPredicate<SpscSlots::Instance<T>, u64, SpscSlots::head<T>> for HeadInv {
    open spec fn atomic_inv(k: SpscSlots::Instance<T>, v: u64, g: SpscSlots::head<T>) -> bool {
        &&& g.instance_id() == k.id()
        &&& g.value() == v as nat
    }
}

/// The `tail` atomic's invariant, likewise.
pub struct TailInv;

impl<T> AtomicInvariantPredicate<SpscSlots::Instance<T>, u64, SpscSlots::tail<T>> for TailInv {
    open spec fn atomic_inv(k: SpscSlots::Instance<T>, v: u64, g: SpscSlots::tail<T>) -> bool {
        &&& g.instance_id() == k.id()
        &&& g.value() == v as nat
    }
}

/// What the two halves share.
pub struct Shared<T> {
    cells: Vec<PCell<T>>,
    ring_bits: u8,
    cap: usize,
    head: AtomicU64<SpscSlots::Instance<T>, SpscSlots::head<T>, HeadInv>,
    tail: AtomicU64<SpscSlots::Instance<T>, SpscSlots::tail<T>, TailInv>,
    // Ghost: erased in plain builds, where nothing reads it.
    #[allow(dead_code)]
    inst: Tracked<SpscSlots::Instance<T>>,
}

impl<T> Shared<T> {
    pub closed spec fn wf(&self) -> bool {
        &&& 1 <= self.ring_bits <= MAX_RING_BITS
        &&& self.cap as int == cap_spec(self.ring_bits)
        &&& self.inst@.cap() == self.cap as nat
        &&& self.cells@.len() == self.cap as nat
        &&& forall |i: int| 0 <= i < self.cap ==> #[trigger] self.cells@[i].id() == self.inst@.cells()[i]
        &&& self.head.well_formed()
        &&& self.head.constant() == self.inst@
        &&& self.tail.well_formed()
        &&& self.tail.constant() == self.inst@
    }

    pub closed spec fn id(&self) -> InstanceId {
        self.inst@.id()
    }

    /// The cell sequence number `s` lives in.
    pub closed spec fn cell_of(&self, s: nat) -> vstd::cell::CellId {
        self.inst@.cells()[(s % (self.cap as nat)) as int]
    }
}

/// The producer's tokens.
pub tracked struct ProducerToks<T> {
    pub cached_head: SpscSlots::cached_head<T>,
    pub reserved: SpscSlots::reserved<T>,
    pub produced: SpscSlots::produced<T>,
}

/// The sending half. Not `Clone`: there is one producer.
pub struct Producer<T> {
    shared: Arc<Shared<T>>,
    /// The producer's copy of `tail`. It is the only writer, so this is
    /// always the published value.
    tail: u64,
    cached_head: u64,
    /// Slots in the open reservation, `[tail, tail + res_len)`.
    res_len: usize,
    /// How many of them `write_next` has filled, from the front.
    filled: usize,
    // Ghost: the reserved slots' tokens, and the protocol's.
    #[allow(dead_code)]
    res: Tracked<Map<nat, PointsTo<T>>>,
    #[allow(dead_code)]
    toks: Tracked<ProducerToks<T>>,
}

/// The consumer's tokens.
pub tracked struct ConsumerToks<T> {
    pub cached_tail: SpscSlots::cached_tail<T>,
    pub held: SpscSlots::held<T>,
    pub held_values: SpscSlots::held_values<T>,
    pub consumed: SpscSlots::consumed<T>,
}

/// The receiving half. Not `Clone`: there is one consumer.
pub struct Consumer<T> {
    shared: Arc<Shared<T>>,
    /// The consumer's copy of `head`. It is the only writer, so this is
    /// always the published value.
    head: u64,
    cached_tail: u64,
    /// Published slots held for reading, `[head, head + held_len)`.
    held_len: usize,
    // Ghost: the held slots' tokens, and the protocol's.
    #[allow(dead_code)]
    held: Tracked<Map<nat, PointsTo<T>>>,
    #[allow(dead_code)]
    toks: Tracked<ConsumerToks<T>>,
}

impl<T> Producer<T> {
    /// Every item this producer has published, in order.
    pub closed spec fn produced(&self) -> Seq<T> {
        self.toks@.produced.value()
    }

    /// Which ring this producer belongs to.
    pub closed spec fn id(&self) -> InstanceId {
        self.shared.id()
    }

    pub closed spec fn capacity_spec(&self) -> nat {
        self.shared.cap as nat
    }

    /// Slots in the open reservation (0: none open).
    pub closed spec fn reserved_len(&self) -> nat {
        self.res_len as nat
    }

    /// How many reserved slots are filled.
    pub closed spec fn filled_len(&self) -> nat {
        self.filled as nat
    }

    /// The items written into the reservation, not yet published.
    pub closed spec fn pending(&self) -> Seq<T> {
        Seq::new(self.filled as nat, |i: int| self.res@[(self.tail + i) as nat].value())
    }

    pub closed spec fn wf(&self) -> bool {
        let t = self.toks@;
        let inst = self.shared.inst@;
        let lo = self.tail as nat;
        let hi = lo + self.res_len as nat;
        &&& self.shared.wf()
        &&& t.cached_head.instance_id() == inst.id()
        &&& t.reserved.instance_id() == inst.id()
        &&& t.produced.instance_id() == inst.id()
        &&& t.produced.value().len() == lo
        &&& t.cached_head.value() == self.cached_head as nat
        &&& t.reserved.value() == self.res_len as nat
        &&& self.filled <= self.res_len
        &&& hi <= u64::MAX
        &&& self.res@.dom() == range(lo, hi)
        &&& forall |s: nat| lo <= s < hi ==> #[trigger] self.res@[s].id() == self.shared.cell_of(s)
        &&& forall |s: nat| lo <= s < lo + self.filled ==> #[trigger] self.res@[s].is_init()
        &&& forall |s: nat| lo + self.filled <= s < hi ==> #[trigger] self.res@[s].is_uninit()
    }

    /// `wf()` without the reservation's tokens, which `commit_up_to` moves
    /// out while it works.
    closed spec fn wf_except_res(&self) -> bool {
        let t = self.toks@;
        let inst = self.shared.inst@;
        &&& self.shared.wf()
        &&& t.cached_head.instance_id() == inst.id()
        &&& t.reserved.instance_id() == inst.id()
        &&& t.produced.instance_id() == inst.id()
        &&& t.produced.value().len() == self.tail as nat
        &&& t.cached_head.value() == self.cached_head as nat
        &&& t.reserved.value() == self.res_len as nat
        &&& self.filled <= self.res_len
        &&& self.tail as nat + self.res_len as nat <= u64::MAX
    }

    pub fn capacity(&self) -> (n: usize)
        requires
            self.wf(),
        ensures
            n == self.capacity_spec(),
    {
        self.shared.cap
    }

    /// Reserve up to `n` slots; returns how many were granted (0 if the
    /// ring is full).
    ///
    /// Mirrors `reserve_shared`: space by `cached_head` first, and reload
    /// `head` from its atomic only when the cache says the ring is full.
    pub fn reserve(&mut self, n: usize) -> (granted: usize)
        requires
            old(self).wf(),
            old(self).reserved_len() == 0,
        ensures
            final(self).wf(),
            final(self).id() == old(self).id(),
            final(self).capacity_spec() == old(self).capacity_spec(),
            final(self).produced() == old(self).produced(),
            granted <= n,
            final(self).reserved_len() == granted,
            final(self).filled_len() == 0,
    {
        proof {
            let tracked t = self.toks.borrow();
            self.shared.inst.borrow().producer_facts(&t.cached_head, &t.produced);
        }
        if n == 0 {
            return 0;
        }
        let cap = self.shared.cap as u64;
        let mut used = self.tail - self.cached_head;
        if used >= cap {
            // Slow path: reload head from its atomic into the cache.
            let h = atomic_with_ghost!(&self.shared.head => load();
                ghost g => {
                    let tracked t = self.toks.borrow_mut();
                    self.shared.inst.borrow().refresh_head(&g, &mut t.cached_head);
                }
            );
            self.cached_head = h;
            proof {
                let tracked t = self.toks.borrow();
                self.shared.inst.borrow().producer_facts(&t.cached_head, &t.produced);
            }
            used = self.tail - self.cached_head;
            if used >= cap {
                return 0;
            }
        }
        let space = cap - used;
        let mut g: u64 = if (n as u64) < space { n as u64 } else { space };
        // Never let `tail + g` overflow (after 2^64 items: INV-SEQ-03).
        if g > u64::MAX - self.tail {
            g = u64::MAX - self.tail;
        }
        if g == 0 {
            return 0;
        }
        let tracked mut m: Map<nat, PointsTo<T>>;
        proof {
            let tracked t = self.toks.borrow_mut();
            let tracked (_, Tracked(w)) = self.shared.inst.borrow().produce_start(g as nat, &t.cached_head, &mut t.reserved, &t.produced);
            m = w;
        }
        self.res = Tracked(m);
        self.res_len = g as usize;
        self.filled = 0;
        proof {
            assert forall |s: nat| self.tail <= s < self.tail + g implies
                #[trigger] self.res@[s].id() == self.shared.cell_of(s) by {}
        }
        g as usize
    }

    /// Write the next reserved slot.
    pub fn write_next(&mut self, v: T)
        requires
            old(self).wf(),
            old(self).filled_len() < old(self).reserved_len(),
        ensures
            final(self).wf(),
            final(self).id() == old(self).id(),
            final(self).capacity_spec() == old(self).capacity_spec(),
            final(self).produced() == old(self).produced(),
            final(self).reserved_len() == old(self).reserved_len(),
            final(self).filled_len() == old(self).filled_len() + 1,
            final(self).pending() == old(self).pending().push(v),
    {
        let key = self.tail + self.filled as u64;
        let idx = slot_index(key, self.shared.ring_bits);
        let tracked mut perm: PointsTo<T>;
        proof {
            assert(self.res@[key as nat].is_uninit());
            perm = self.res.borrow_mut().tracked_remove(key as nat);
            assert(self.shared.cells@[idx as int].id() == self.shared.cell_of(key as nat));
        }
        self.shared.cells[idx].put(Tracked(&mut perm), v);
        proof {
            self.res.borrow_mut().tracked_insert(key as nat, perm);
        }
        self.filled = self.filled + 1;
        proof {
            assert(self.res@.dom() =~= range(self.tail as nat, self.tail as nat + self.res_len as nat));
            assert(self.pending() =~= old(self).pending().push(v));
        }
    }

    /// Publish the first `k` filled slots (`try_commit_n`, `commit_up_to`),
    /// with `tail.store(tail + k)`. Filled slots past `k` are dropped; the
    /// reservation closes. Returns how many were published.
    pub fn commit_up_to(&mut self, k: usize) -> (c: usize)
        requires
            old(self).wf(),
            old(self).reserved_len() > 0,
        ensures
            final(self).wf(),
            final(self).id() == old(self).id(),
            final(self).capacity_spec() == old(self).capacity_spec(),
            final(self).reserved_len() == 0,
            c == if k < old(self).filled_len() { k as nat } else { old(self).filled_len() },
            final(self).produced() == old(self).produced() + old(self).pending().take(c as int),
    {
        let c = if k < self.filled { k } else { self.filled };
        let tracked mut m: Map<nat, PointsTo<T>> = Map::tracked_empty();
        proof {
            vstd::modes::tracked_swap(&mut m, self.res.borrow_mut());
        }
        let ghost old_m = m;
        let ghost lo = self.tail as nat;
        let ghost hi = lo + self.res_len as nat;
        // Drop what was written past `c`: those slots stay unpublished.
        let mut j = c;
        while j < self.filled
            invariant
                self.wf_except_res(),
                self.tail == old(self).tail,
                self.res_len == old(self).res_len,
                self.filled == old(self).filled,
                self.shared == old(self).shared,
                self.toks == old(self).toks,
                lo == self.tail as nat,
                hi == lo + self.res_len as nat,
                c <= j <= self.filled,
                m.dom() == range(lo, hi),
                forall |s: nat| lo <= s < hi ==> #[trigger] m[s].id() == self.shared.cell_of(s),
                forall |s: nat| lo <= s < lo + c ==> #[trigger] m[s] == old_m[s],
                forall |s: nat| lo + c <= s < lo + j ==> #[trigger] m[s].is_uninit(),
                forall |s: nat| lo + j <= s < hi ==> #[trigger] m[s] == old_m[s],
                forall |s: nat| lo <= s < lo + self.filled ==> #[trigger] old_m[s].is_init(),
                forall |s: nat| lo + self.filled <= s < hi ==> #[trigger] old_m[s].is_uninit(),
            decreases self.filled - j,
        {
            let key = self.tail + j as u64;
            let idx = slot_index(key, self.shared.ring_bits);
            let tracked mut perm: PointsTo<T>;
            proof {
                perm = m.tracked_remove(key as nat);
                assert(self.shared.cells@[idx as int].id() == self.shared.cell_of(key as nat));
            }
            let _dropped = self.shared.cells[idx].take(Tracked(&mut perm));
            proof {
                m.tracked_insert(key as nat, perm);
                assert(m.dom() =~= range(lo, hi));
            }
            j = j + 1;
        }
        let new_tail = self.tail + c as u64;
        // Publish: tail.store(tail + c).
        atomic_with_ghost!(&self.shared.tail => store(new_tail);
            ghost g => {
                let tracked t = self.toks.borrow_mut();
                self.shared.inst.borrow().tail_is_produced_len(&g, &t.produced);
                self.shared.inst.borrow().produce_end(c as nat, m, m, &mut g, &mut t.reserved, &mut t.produced);
            }
        );
        proof {
            assert(Seq::new(c as nat, |i: int| m[(lo + i) as nat].value()) =~= old(self).pending().take(c as int));
        }
        self.tail = new_tail;
        self.res_len = 0;
        self.filled = 0;
        proof {
            assert(self.res@.dom() =~= range(self.tail as nat, self.tail as nat));
        }
        c
    }

    /// Publish every filled slot (`Reservation::commit`).
    pub fn commit(&mut self) -> (c: usize)
        requires
            old(self).wf(),
            old(self).reserved_len() > 0,
        ensures
            final(self).wf(),
            final(self).id() == old(self).id(),
            final(self).capacity_spec() == old(self).capacity_spec(),
            final(self).reserved_len() == 0,
            c == old(self).filled_len(),
            final(self).produced() == old(self).produced() + old(self).pending(),
    {
        let n = self.filled;
        let c = self.commit_up_to(n);
        proof {
            assert(old(self).pending().take(c as int) =~= old(self).pending());
        }
        c
    }

    /// Close the reservation without publishing anything (a `Reservation`
    /// dropped without committing). Written items are dropped.
    pub fn abandon(&mut self)
        requires
            old(self).wf(),
            old(self).reserved_len() > 0,
        ensures
            final(self).wf(),
            final(self).id() == old(self).id(),
            final(self).capacity_spec() == old(self).capacity_spec(),
            final(self).reserved_len() == 0,
            final(self).produced() == old(self).produced(),
    {
        let _ = self.commit_up_to(0);
        proof {
            assert(old(self).produced() + old(self).pending().take(0) =~= old(self).produced());
        }
    }

    /// Send `v`, or hand it back if the ring is full (or, after 2^64 items,
    /// exhausted): a reservation of one.
    pub fn push(&mut self, v: T) -> (r: Result<(), T>)
        requires
            old(self).wf(),
            old(self).reserved_len() == 0,
        ensures
            final(self).wf(),
            final(self).id() == old(self).id(),
            final(self).capacity_spec() == old(self).capacity_spec(),
            final(self).reserved_len() == 0,
            r is Ok ==> final(self).produced() == old(self).produced().push(v),
            r is Err ==> final(self).produced() == old(self).produced() && r->Err_0 == v,
    {
        if self.reserve(1) == 0 {
            return Err(v);
        }
        self.write_next(v);
        let ghost p = self.pending();
        let _ = self.commit_up_to(1);
        proof {
            assert(p =~= seq![v]);
            assert(self.produced() =~= old(self).produced().push(v));
        }
        Ok(())
    }
}

impl<T> Consumer<T> {
    /// Every item this consumer has consumed, in order.
    pub closed spec fn consumed(&self) -> Seq<T> {
        self.toks@.consumed.value()
    }

    /// The published items it holds for reading, oldest first.
    pub closed spec fn held_view(&self) -> Seq<T> {
        self.toks@.held_values.value()
    }

    pub closed spec fn held_len_spec(&self) -> nat {
        self.held_len as nat
    }

    /// Which ring this consumer belongs to.
    pub closed spec fn id(&self) -> InstanceId {
        self.shared.id()
    }

    pub closed spec fn capacity_spec(&self) -> nat {
        self.shared.cap as nat
    }

    pub closed spec fn wf(&self) -> bool {
        let t = self.toks@;
        let inst = self.shared.inst@;
        let lo = self.head as nat;
        let hi = lo + self.held_len as nat;
        &&& self.shared.wf()
        &&& t.cached_tail.instance_id() == inst.id()
        &&& t.held.instance_id() == inst.id()
        &&& t.held_values.instance_id() == inst.id()
        &&& t.consumed.instance_id() == inst.id()
        &&& t.consumed.value().len() == lo
        &&& t.cached_tail.value() == self.cached_tail as nat
        &&& t.held.value() == self.held_len as nat
        &&& t.held_values.value().len() == self.held_len as nat
        &&& self.held_len <= self.shared.cap
        &&& self.held_len > 0 ==> hi <= self.cached_tail as nat
        &&& self.held@.dom() == range(lo, hi)
        &&& forall |s: nat| lo <= s < hi ==> {
            &&& #[trigger] self.held@[s].id() == self.shared.cell_of(s)
            &&& self.held@[s].is_init()
            &&& self.held@[s].value() == t.held_values.value()[s - lo]
        }
    }

    pub fn capacity(&self) -> (n: usize)
        requires
            self.wf(),
        ensures
            n == self.capacity_spec(),
    {
        self.shared.cap
    }

    /// Hold every published item for reading; returns how many are held.
    ///
    /// Mirrors `readable`'s fixed fast path (#17): read by `cached_tail`
    /// only when `cached_tail > head`, otherwise reload `tail` from its
    /// atomic.
    pub fn readable(&mut self) -> (n: usize)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            final(self).id() == old(self).id(),
            final(self).capacity_spec() == old(self).capacity_spec(),
            final(self).consumed() == old(self).consumed(),
            n == final(self).held_len_spec(),
            old(self).held_len_spec() > 0 ==> final(self).held_view() == old(self).held_view() && n == old(self).held_len_spec(),
    {
        if self.held_len > 0 {
            return self.held_len;
        }
        if self.cached_tail <= self.head {
            // Slow path: reload tail from its atomic into the cache.
            let t_now = atomic_with_ghost!(&self.shared.tail => load();
                ghost g => {
                    let tracked t = self.toks.borrow_mut();
                    self.shared.inst.borrow().refresh_tail(&g, &mut t.cached_tail);
                }
            );
            self.cached_tail = t_now;
            if self.cached_tail <= self.head {
                return 0;
            }
        }
        let n = self.cached_tail - self.head;
        let tracked mut m: Map<nat, PointsTo<T>>;
        proof {
            let tracked t = self.toks.borrow_mut();
            let tracked (_, Tracked(w)) = self.shared.inst.borrow().consume_start(n as nat, &t.cached_tail, &mut t.held, &mut t.held_values, &t.consumed);
            m = w;
        }
        self.held = Tracked(m);
        self.held_len = n as usize;
        proof {
            assert forall |s: nat| self.head <= s < self.head + n implies {
                &&& #[trigger] self.held@[s].id() == self.shared.cell_of(s)
                &&& self.held@[s].is_init()
                &&& self.held@[s].value() == self.toks@.held_values.value()[s - self.head]
            } by {}
        }
        self.held_len
    }

    /// Borrow held item `i` in place: zero-copy access to the ring slot.
    pub fn get(&self, i: usize) -> (r: &T)
        requires
            self.wf(),
            i < self.held_len_spec(),
        ensures
            *r == self.held_view()[i as int],
    {
        let key = self.head + i as u64;
        let idx = slot_index(key, self.shared.ring_bits);
        let tracked p = self.held.borrow().tracked_borrow(key as nat);
        proof {
            assert(self.shared.cells@[idx as int].id() == self.shared.cell_of(key as nat));
        }
        self.shared.cells[idx].borrow(Tracked(p))
    }

    /// Consume the first `k` held items, in order, with
    /// `head.store(head + k)`; the rest go back to the ring.
    pub fn release(&mut self, k: usize) -> (out: Vec<T>)
        requires
            old(self).wf(),
            old(self).held_len_spec() > 0,
            k <= old(self).held_len_spec(),
        ensures
            final(self).wf(),
            final(self).id() == old(self).id(),
            final(self).capacity_spec() == old(self).capacity_spec(),
            final(self).held_len_spec() == 0,
            out@ == old(self).held_view().take(k as int),
            final(self).consumed() == old(self).consumed() + out@,
    {
        let tracked mut m: Map<nat, PointsTo<T>> = Map::tracked_empty();
        proof {
            vstd::modes::tracked_swap(&mut m, self.held.borrow_mut());
        }
        let ghost old_m = m;
        let ghost lo = self.head as nat;
        let ghost hi = lo + self.held_len as nat;
        let ghost cap = self.shared.cap as nat;
        let ghost hv = self.held_view();
        let mut out: Vec<T> = Vec::new();
        let mut j: usize = 0;
        while j < k
            invariant
                self.shared == old(self).shared,
                self.shared.wf(),
                self.head == old(self).head,
                self.held_len == old(self).held_len,
                self.cached_tail == old(self).cached_tail,
                self.toks == old(self).toks,
                lo == self.head as nat,
                hi == lo + self.held_len as nat,
                cap == self.shared.cap as nat,
                hv == self.held_view(),
                hv.len() == self.held_len as nat,
                hi <= u64::MAX,
                self.held_len <= self.shared.cap,
                k <= self.held_len,
                j <= k,
                m.dom() == release_keys(lo, self.held_len as nat, j as nat, cap),
                forall |s: nat| lo + j <= s < hi ==> #[trigger] m[s] == old_m[s],
                forall |s: nat| lo + cap <= s < lo + cap + j ==> {
                    &&& #[trigger] m[s].id() == self.shared.cell_of(s)
                    &&& m[s].is_uninit()
                },
                forall |s: nat| lo <= s < hi ==> {
                    &&& #[trigger] old_m[s].id() == self.shared.cell_of(s)
                    &&& old_m[s].is_init()
                    &&& old_m[s].value() == hv[s - lo]
                },
                out@ == hv.take(j as int),
            decreases k - j,
        {
            let key = self.head + j as u64;
            let idx = slot_index(key, self.shared.ring_bits);
            let ghost m_prev = m;
            let tracked mut perm: PointsTo<T>;
            proof {
                assert(m[key as nat] == old_m[key as nat]);
                assert(old_m[key as nat].id() == self.shared.cell_of(key as nat));
                assert(old_m[key as nat].is_init());
                assert(old_m[key as nat].value() == hv[j as int]);
                perm = m.tracked_remove(key as nat);
                assert(self.shared.cells@[idx as int].id() == self.shared.cell_of(key as nat));
            }
            let v = self.shared.cells[idx].take(Tracked(&mut perm));
            proof {
                assert(v == hv[j as int]);
            }
            proof {
                crate::slots::lemma_next_lap_same_cell(key as nat, cap);
                assert(self.shared.cell_of((key + cap) as nat) == self.shared.cell_of(key as nat));
                m.tracked_insert((key + cap) as nat, perm);
                assert(m.dom() =~= release_keys(lo, self.held_len as nat, (j + 1) as nat, cap));
                assert forall |s: nat| lo + cap <= s < lo + cap + j + 1 implies {
                    &&& #[trigger] m[s].id() == self.shared.cell_of(s)
                    &&& m[s].is_uninit()
                } by {
                    if s != (key + cap) as nat {
                        assert(m[s] == m_prev[s]);
                    }
                }
                assert forall |s: nat| lo + j + 1 <= s < hi implies #[trigger] m[s] == old_m[s] by {
                    assert(m[s] == m_prev[s]);
                }
            }
            out.push(v);
            proof {
                assert(out@ =~= hv.take(j + 1));
            }
            j = j + 1;
        }
        proof {
            // What consume_end needs: the unconsumed slots are untouched.
            assert forall |s: nat| lo + k <= s < hi implies {
                &&& #[trigger] m[s].id() == self.shared.cell_of(s)
                &&& m[s].is_init()
                &&& m[s].value() == hv[s - lo]
            } by {
                assert(m[s] == old_m[s]);
                assert(old_m[s].id() == self.shared.cell_of(s));
            }
        }
        let new_head = self.head + k as u64;
        // Release: head.store(head + k).
        atomic_with_ghost!(&self.shared.head => store(new_head);
            ghost g => {
                let tracked t = self.toks.borrow_mut();
                self.shared.inst.borrow().head_is_consumed_len(&g, &t.consumed);
                self.shared.inst.borrow().consume_end(k as nat, m, m, &mut g, &mut t.held, &mut t.held_values, &mut t.consumed);
            }
        );
        self.head = new_head;
        self.held_len = 0;
        proof {
            assert(self.held@.dom() =~= range(self.head as nat, self.head as nat));
        }
        out
    }

    /// Consume the first `k` held items, dropping them (`Ring::advance`).
    pub fn advance(&mut self, k: usize)
        requires
            old(self).wf(),
            old(self).held_len_spec() > 0,
            k <= old(self).held_len_spec(),
        ensures
            final(self).wf(),
            final(self).id() == old(self).id(),
            final(self).capacity_spec() == old(self).capacity_spec(),
            final(self).held_len_spec() == 0,
            final(self).consumed() == old(self).consumed() + old(self).held_view().take(k as int),
    {
        let _dropped = self.release(k);
    }

    /// Receive the oldest item, if one has been published.
    pub fn pop(&mut self) -> (r: Option<T>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            final(self).id() == old(self).id(),
            final(self).capacity_spec() == old(self).capacity_spec(),
            r is None ==> final(self).consumed() == old(self).consumed(),
            r is Some ==> final(self).consumed() == old(self).consumed().push(r->Some_0),
    {
        let n = self.readable();
        if n == 0 {
            return None;
        }
        let mut out = self.release(1);
        let ghost before = out@;
        match out.pop() {
            Some(v) => {
                proof {
                    assert(before =~= seq![v]);
                }
                Some(v)
            },
            None => {
                proof { assert(false); }
                None
            },
        }
    }

    /// `consume_up_to_owned`: up to `max` of the published items, oldest
    /// first.
    pub fn consume_up_to(&mut self, max: usize) -> (out: Vec<T>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            final(self).id() == old(self).id(),
            final(self).capacity_spec() == old(self).capacity_spec(),
            out@.len() <= max,
            final(self).consumed() == old(self).consumed() + out@,
    {
        let n = self.readable();
        if n == 0 {
            let out = Vec::new();
            proof {
                assert(old(self).consumed() + out@ =~= old(self).consumed());
            }
            return out;
        }
        let k = if max < n { max } else { n };
        self.release(k)
    }

    /// `recv`: copy up to `max` published items out (`T: Copy`), read in
    /// place through `get`, then consume them.
    pub fn recv(&mut self, out: &mut Vec<T>, max: usize) -> (n: usize)
        where T: Copy
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            final(self).id() == old(self).id(),
            final(self).capacity_spec() == old(self).capacity_spec(),
            n <= max,
            final(self).consumed() == old(self).consumed() + final(out)@.skip(old(out)@.len() as int),
            final(out)@.take(old(out)@.len() as int) == old(out)@,
            final(out)@.len() == old(out)@.len() + n,
    {
        let avail = self.readable();
        if avail == 0 {
            proof {
                assert(out@.skip(old(out)@.len() as int) =~= Seq::<T>::empty());
                assert(self.consumed() + Seq::<T>::empty() =~= self.consumed());
                assert(out@.take(old(out)@.len() as int) =~= old(out)@);
            }
            return 0;
        }
        let k = if max < avail { max } else { avail };
        let ghost hv = self.held_view();
        let mut i: usize = 0;
        while i < k
            invariant
                self.wf(),
                self.id() == old(self).id(),
                self.capacity_spec() == old(self).capacity_spec(),
                self.consumed() == old(self).consumed(),
                self.held_view() == hv,
                self.held_len_spec() > 0,
                k <= self.held_len_spec(),
                i <= k,
                out@ == old(out)@ + hv.take(i as int),
            decreases k - i,
        {
            let v = *self.get(i);
            out.push(v);
            proof {
                assert(out@ =~= old(out)@ + hv.take(i + 1));
            }
            i = i + 1;
        }
        self.advance(k);
        proof {
            assert(out@.skip(old(out)@.len() as int) =~= hv.take(k as int));
            assert(out@.take(old(out)@.len() as int) =~= old(out)@);
        }
        k
    }
}

/// FIFO across threads: whatever a consumer has consumed is a prefix of
/// what its producer has published, in the same order. Callable once both
/// halves are in hand again, for example after joining their threads.
///
/// Its body is proof only, so plain builds erase it to a no-op.
#[allow(unused_variables)]
pub fn fifo_holds<T>(p: &Producer<T>, c: &Consumer<T>)
    requires
        p.wf(),
        c.wf(),
        p.id() == c.id(),
    ensures
        is_prefix(c.consumed(), p.produced()),
{
    proof {
        p.shared.inst.borrow().fifo_holds(&p.toks.borrow().produced, &c.toks.borrow().consumed);
    }
}

/// A new ring with `2^ring_bits` slots, as a producer half and a consumer
/// half.
pub fn channel<T>(ring_bits: u8) -> (r: (Producer<T>, Consumer<T>))
    requires
        1 <= ring_bits <= MAX_RING_BITS,
    ensures
        r.0.wf(),
        r.1.wf(),
        r.0.id() == r.1.id(),
        r.0.produced() == Seq::<T>::empty(),
        r.1.consumed() == Seq::<T>::empty(),
        r.0.reserved_len() == 0,
        r.0.capacity_spec() == cap_spec(ring_bits),
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
        Tracked(reserved),
        Tracked(held),
        Tracked(held_values),
        Tracked(produced),
        Tracked(consumed),
    ) = SpscSlots::Instance::initialize(cap as nat, ids, perms, perms);
    let head_atomic = AtomicU64::new(Ghost(inst), 0, Tracked(head));
    let tail_atomic = AtomicU64::new(Ghost(inst), 0, Tracked(tail));
    let shared = Arc::new(Shared {
        cells,
        ring_bits,
        cap,
        head: head_atomic,
        tail: tail_atomic,
        inst: Tracked(inst),
    });
    let tracked res: Map<nat, PointsTo<T>> = Map::tracked_empty();
    let tracked hold: Map<nat, PointsTo<T>> = Map::tracked_empty();
    let producer = Producer {
        shared: arc_clone(&shared),
        tail: 0,
        cached_head: 0,
        res_len: 0,
        filled: 0,
        res: Tracked(res),
        toks: Tracked(ProducerToks { cached_head, reserved, produced }),
    };
    let consumer = Consumer {
        shared,
        head: 0,
        cached_tail: 0,
        held_len: 0,
        held: Tracked(hold),
        toks: Tracked(ConsumerToks { cached_tail, held, held_values, consumed }),
    };
    proof {
        assert(producer.res@.dom() =~= range(0, 0));
        assert(consumer.held@.dom() =~= range(0, 0));
    }
    (producer, consumer)
}

} // verus!

/// Dropping the consumer drops the items still published in the ring, as
/// the production ring's `Drop` does. Without this they would leak (safely:
/// a `PCell` never drops its contents).
///
/// Trusted glue, outside `verus!`: it calls the verified `consume_up_to`,
/// whose precondition `wf()` holds for every `Consumer` safe code can
/// obtain. The fields are private, `channel` establishes `wf()`, and every
/// method preserves it. Items the producer publishes after this runs are
/// leaked, not dropped.
impl<T> Drop for Consumer<T> {
    fn drop(&mut self) {
        let cap = self.capacity();
        let _ = self.consume_up_to(cap);
    }
}
