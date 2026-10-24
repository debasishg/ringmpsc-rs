//! A verified concurrent SPSC ring (phase 4, #23).
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
//! `cached_head`, `producing` and `produced` history, the consumer its
//! `cached_tail`, `consuming` and `consumed` history. Neither is `Clone`,
//! and every method takes `&mut self`, so there is exactly one of each,
//! the property #12 enforces in the production crate.
//!
//! Every protocol step that touches a shared cursor happens inside an
//! atomic operation on it (`atomic_with_ghost!`), the same points where the
//! production code does its `load`s and `store`s. So the proofs hold under
//! any interleaving of the two threads.
//!
//! **Orderings:** `vstd` atomics are sequentially consistent, and Verus
//! reasons about them that way. The proof shows the protocol is race-free.
//! It does not show that the production code's `Relaxed`/`Acquire`/`Release`
//! are enough; that stays with loom and Miri.

use std::sync::Arc;
use vstd::atomic_ghost::*;
use vstd::cell::pcell_maybe_uninit::PCell;
use vstd::prelude::*;
use crate::arith::{capacity, slot_index};
use crate::slots::SpscSlots;
#[cfg(verus_only)]
use crate::arith::{cap_spec, MAX_RING_BITS};
#[cfg(verus_only)]
use crate::slots::is_prefix;
#[cfg(verus_only)]
use vstd::cell::pcell_maybe_uninit::PointsTo;

verus! {

/// `Arc::clone` points at the same value. `vstd` has no spec for it, so
/// this one-line wrapper states it. It is this crate's only trusted
/// function outside `vstd`.
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
}

/// The producer's tokens.
pub tracked struct ProducerToks<T> {
    pub cached_head: SpscSlots::cached_head<T>,
    pub producing: SpscSlots::producing<T>,
    pub produced: SpscSlots::produced<T>,
}

/// The sending half. Not `Clone`: there is one producer.
pub struct Producer<T> {
    shared: Arc<Shared<T>>,
    /// The producer's copy of `tail`. It is the only writer, so this is
    /// always the published value.
    tail: u64,
    cached_head: u64,
    // Ghost: erased in plain builds, where nothing reads it.
    #[allow(dead_code)]
    toks: Tracked<ProducerToks<T>>,
}

/// The consumer's tokens.
pub tracked struct ConsumerToks<T> {
    pub cached_tail: SpscSlots::cached_tail<T>,
    pub consuming: SpscSlots::consuming<T>,
    pub consumed: SpscSlots::consumed<T>,
}

/// The receiving half. Not `Clone`: there is one consumer.
pub struct Consumer<T> {
    shared: Arc<Shared<T>>,
    /// The consumer's copy of `head`. It is the only writer, so this is
    /// always the published value.
    head: u64,
    cached_tail: u64,
    // Ghost: erased in plain builds, where nothing reads it.
    #[allow(dead_code)]
    toks: Tracked<ConsumerToks<T>>,
}

impl<T> Producer<T> {
    /// Every item this producer has sent, in order.
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

    pub closed spec fn wf(&self) -> bool {
        let t = self.toks@;
        let inst = self.shared.inst@;
        &&& self.shared.wf()
        &&& t.cached_head.instance_id() == inst.id()
        &&& t.producing.instance_id() == inst.id()
        &&& t.produced.instance_id() == inst.id()
        &&& t.produced.value().len() == self.tail as nat
        &&& t.cached_head.value() == self.cached_head as nat
        &&& !t.producing.value()
    }

    pub fn capacity(&self) -> (n: usize)
        requires
            self.wf(),
        ensures
            n == self.capacity_spec(),
    {
        self.shared.cap
    }

    /// Send `v`, or hand it back if the ring is full (or, after 2^64 items,
    /// exhausted).
    ///
    /// Mirrors `reserve_shared`: check space against `cached_head`, and
    /// reload `head` from its atomic only when the cache says the ring is
    /// full.
    pub fn push(&mut self, v: T) -> (r: Result<(), T>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            final(self).id() == old(self).id(),
            final(self).capacity_spec() == old(self).capacity_spec(),
            r is Ok ==> final(self).produced() == old(self).produced().push(v),
            r is Err ==> final(self).produced() == old(self).produced() && r->Err_0 == v,
    {
        proof {
            let tracked t = self.toks.borrow();
            self.shared.inst.borrow().producer_facts(&t.cached_head, &t.produced);
        }
        if self.tail == u64::MAX {
            return Err(v);
        }
        let cap = self.shared.cap as u64;
        if self.tail - self.cached_head >= cap {
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
            if self.tail - self.cached_head >= cap {
                return Err(v);
            }
        }
        let idx = slot_index(self.tail, self.shared.ring_bits);
        let tracked mut perm: PointsTo<T>;
        proof {
            let tracked t = self.toks.borrow_mut();
            perm = self.shared.inst.borrow().produce_start(&t.cached_head, &mut t.producing, &t.produced);
        }
        self.shared.cells[idx].put(Tracked(&mut perm), v);
        let new_tail = self.tail + 1;
        // Publish: tail.store(tail + 1).
        atomic_with_ghost!(&self.shared.tail => store(new_tail);
            ghost g => {
                let tracked t = self.toks.borrow_mut();
                self.shared.inst.borrow().tail_is_produced_len(&g, &t.produced);
                self.shared.inst.borrow().produce_end(perm, perm, &mut g, &mut t.producing, &mut t.produced);
            }
        );
        self.tail = new_tail;
        Ok(())
    }
}

impl<T> Consumer<T> {
    /// Every item this consumer has received, in order.
    pub closed spec fn consumed(&self) -> Seq<T> {
        self.toks@.consumed.value()
    }

    /// Which ring this consumer belongs to.
    pub closed spec fn id(&self) -> InstanceId {
        self.shared.id()
    }

    pub closed spec fn wf(&self) -> bool {
        let t = self.toks@;
        let inst = self.shared.inst@;
        &&& self.shared.wf()
        &&& t.cached_tail.instance_id() == inst.id()
        &&& t.consuming.instance_id() == inst.id()
        &&& t.consumed.instance_id() == inst.id()
        &&& t.consumed.value().len() == self.head as nat
        &&& t.cached_tail.value() == self.cached_tail as nat
        &&& !t.consuming.value()
    }

    pub closed spec fn capacity_spec(&self) -> nat {
        self.shared.cap as nat
    }

    pub fn capacity(&self) -> (n: usize)
        requires
            self.wf(),
        ensures
            n == self.capacity_spec(),
    {
        self.shared.cap
    }

    /// Receive the oldest item, if one has been published.
    ///
    /// Mirrors `readable`'s fixed fast path (#17): read by `cached_tail`
    /// only when `cached_tail > head`, otherwise reload `tail` from its
    /// atomic.
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
                return None;
            }
        }
        let idx = slot_index(self.head, self.shared.ring_bits);
        let tracked mut perm: PointsTo<T>;
        proof {
            let tracked t = self.toks.borrow_mut();
            perm = self.shared.inst.borrow().consume_start(&t.cached_tail, &mut t.consuming, &mut t.consumed);
        }
        let v = self.shared.cells[idx].take(Tracked(&mut perm));
        let new_head = self.head + 1;
        // Release the slot: head.store(head + 1).
        atomic_with_ghost!(&self.shared.head => store(new_head);
            ghost g => {
                let tracked t = self.toks.borrow_mut();
                self.shared.inst.borrow().head_is_consumed_len(&g, &t.consuming, &t.consumed);
                self.shared.inst.borrow().consume_end(perm, perm, &mut g, &mut t.consuming);
            }
        );
        self.head = new_head;
        Some(v)
    }
}

impl<T> Consumer<T> {
    /// `consume_up_to_owned`: receive up to `max` items, oldest first.
    /// Stops early at the first moment nothing is published.
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
        let mut out: Vec<T> = Vec::new();
        while out.len() < max
            invariant
                self.wf(),
                self.id() == old(self).id(),
                self.capacity_spec() == old(self).capacity_spec(),
                out@.len() <= max,
                self.consumed() == old(self).consumed() + out@,
            decreases max - out.len(),
        {
            match self.pop() {
                Some(item) => {
                    out.push(item);
                    proof {
                        assert(self.consumed() =~= old(self).consumed() + out@);
                    }
                },
                None => break,
            }
        }
        out
    }
}

/// FIFO across threads: whatever a consumer has received is a prefix of
/// what its producer has sent, in the same order. Callable once both halves
/// are in hand again, for example after joining their threads.
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
        Tracked(producing),
        Tracked(consuming),
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
    let producer = Producer {
        shared: arc_clone(&shared),
        tail: 0,
        cached_head: 0,
        toks: Tracked(ProducerToks { cached_head, producing, produced }),
    };
    let consumer = Consumer {
        shared,
        head: 0,
        cached_tail: 0,
        toks: Tracked(ConsumerToks { cached_tail, consuming, consumed }),
    };
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
