//! A verified MPSC channel (phase 6, #25).
//!
//! The same decomposition as `crates/ringmpsc/src/channel.rs`: N SPSC rings,
//! one per producer, and one consumer over all of them. Each ring is a
//! verified `spsc` channel (phase 4). What this module adds is the handout:
//!
//! - **`register()`** gives each ring's `Producer` half to exactly one
//!   caller, from any thread. The halves sit in `PCell`s, and the
//!   `producer_count` atomic's invariant holds the permissions for the
//!   unclaimed ones, `[count, N)`. A compare-and-swap from `c` to `c + 1`
//!   withdraws permission `c`, so ring ids are handed out once each, in
//!   order, and never after the last.
//! - **`consumer()`** gives all N `Consumer` halves to at most one caller at
//!   a time. They sit in one `PCell`, and the `consumer_taken` flag's
//!   invariant holds its permission while the flag is `false`. Claiming
//!   swaps the flag to `true` and withdraws the permission. Releasing puts
//!   the halves back and stores `false` with the permission. This is the
//!   claim from #12, with its exclusivity proven.
//!
//! Unique producers and the single consumer are linearity facts: a ring's
//! producer and consumer halves own tokens that can't be duplicated, and a
//! cell's permission exists once. Per-producer FIFO (INV-CH-03) is phase 4's
//! `fifo_holds` on that ring's two halves.

use std::sync::Arc;
use vstd::atomic_ghost::*;
// `PointsTo`, `CellId` and `InstanceId` appear in types (the atomics' ghost
// parameters), which plain builds keep, so these imports aren't gated.
use vstd::cell::pcell_maybe_uninit::{PCell, PointsTo};
use vstd::cell::CellId;
use vstd::prelude::*;
use vstd::tokens::InstanceId;
use crate::spsc::{arc_clone, channel as spsc_channel, Consumer, Producer};
#[cfg(verus_only)]
use crate::arith::MAX_RING_BITS;
#[cfg(verus_only)]
use crate::slots::is_prefix;

verus! {

/// The consumer halves of every ring, in ring order, each well-formed.
pub open spec fn consumers_ok<T>(cs: Seq<Consumer<T>>, rings: Seq<InstanceId>) -> bool {
    &&& cs.len() == rings.len()
    &&& forall |i: int| 0 <= i < cs.len() ==> {
        &&& #[trigger] cs[i].wf()
        &&& cs[i].id() == rings[i]
    }
}

/// The registration counter's invariant. `k` is (cell ids, ring ids). The
/// atomic's value `v` is the number of producers handed out; the
/// permissions for the rest, `[v, N)`, live here.
pub struct RegInv;

impl<T> AtomicInvariantPredicate<(Seq<CellId>, Seq<InstanceId>), u64, Map<nat, PointsTo<Producer<T>>>> for RegInv {
    open spec fn atomic_inv(k: (Seq<CellId>, Seq<InstanceId>), v: u64, g: Map<nat, PointsTo<Producer<T>>>) -> bool {
        &&& k.0.len() == k.1.len()
        &&& v <= k.0.len()
        &&& forall |i: nat| #[trigger] g.dom().contains(i) <==> v <= i < k.0.len()
        &&& forall |i: nat| v <= i < k.0.len() ==> {
            &&& #[trigger] g[i].id() == k.0[i as int]
            &&& g[i].is_init()
            &&& g[i].value().wf()
            &&& g[i].value().id() == k.1[i as int]
        }
    }
}

/// The consumer claim's invariant. `k` is (the consumers cell's id, ring
/// ids). While the flag is `false`, the cell's permission lives here and
/// the cell holds every ring's consumer half.
pub struct ClaimInv;

impl<T> AtomicInvariantPredicate<(CellId, Seq<InstanceId>), bool, Option<PointsTo<Vec<Consumer<T>>>>> for ClaimInv {
    open spec fn atomic_inv(k: (CellId, Seq<InstanceId>), v: bool, g: Option<PointsTo<Vec<Consumer<T>>>>) -> bool {
        if v {
            g is None
        } else {
            &&& g is Some
            &&& g->Some_0.id() == k.0
            &&& g->Some_0.is_init()
            &&& consumers_ok(g->Some_0.value()@, k.1)
        }
    }
}

/// What every `Channel` handle shares.
pub struct ChannelShared<T> {
    producers: Vec<PCell<Producer<T>>>,
    consumers: PCell<Vec<Consumer<T>>>,
    producer_count: AtomicU64<(Seq<CellId>, Seq<InstanceId>), Map<nat, PointsTo<Producer<T>>>, RegInv>,
    consumer_taken: AtomicBool<(CellId, Seq<InstanceId>), Option<PointsTo<Vec<Consumer<T>>>>, ClaimInv>,
    n: usize,
    // Ghost: erased in plain builds, where nothing reads it.
    #[allow(dead_code)]
    rings: Ghost<Seq<InstanceId>>,
}

impl<T> ChannelShared<T> {
    pub closed spec fn wf(&self) -> bool {
        &&& self.n == self.producers@.len()
        &&& self.rings@.len() == self.n
        &&& self.producer_count.well_formed()
        &&& self.producer_count.constant() == (self.producers@.map_values(|c: PCell<Producer<T>>| c.id()), self.rings@)
        &&& self.consumer_taken.well_formed()
        &&& self.consumer_taken.constant() == (self.consumers.id(), self.rings@)
    }
}

impl<T> ChannelShared<T> {
    /// See `Channel::register`.
    #[verifier::exec_allows_no_decreases_clause]
    fn register(&self) -> (r: Option<(usize, Producer<T>)>)
        requires
            self.wf(),
        ensures
            r matches Some((id, p)) ==> {
                &&& id < self.rings@.len()
                &&& p.wf()
                &&& p.id() == self.rings@[id as int]
            },
    {
        loop
            invariant
                self.wf(),
        {
            let c = atomic_with_ghost!(&self.producer_count => load(); ghost g => {});
            if c >= self.n as u64 {
                return None;
            }
            let tracked mut claimed: Option<PointsTo<Producer<T>>> = None;
            let res = atomic_with_ghost!(&self.producer_count => compare_exchange(c, c + 1);
                returning res;
                ghost g => {
                    if res is Ok {
                        claimed = Some(g.tracked_remove(c as nat));
                    }
                }
            );
            if let Ok(_) = res {
                let tracked mut perm = claimed.tracked_unwrap();
                let id = c as usize;
                let p = self.producers[id].take(Tracked(&mut perm));
                return Some((id, p));
            }
        }
    }

    /// Take every ring's consumer half out of the claim cell, unless a
    /// consumer is already claimed. The returned permission is the cell's,
    /// now uninitialized; whoever holds it may put the halves back.
    fn claim_consumers(&self) -> (r: Option<(Vec<Consumer<T>>, Tracked<PointsTo<Vec<Consumer<T>>>>)>)
        requires
            self.wf(),
        ensures
            r matches Some((cs, perm)) ==> {
                &&& consumers_ok(cs@, self.rings@)
                &&& perm@.id() == self.consumers.id()
                &&& perm@.is_uninit()
            },
    {
        let tracked mut claimed: Option<PointsTo<Vec<Consumer<T>>>> = None;
        let was_taken = atomic_with_ghost!(&self.consumer_taken => swap(true);
            update prev -> next;
            ghost g => {
                if !prev {
                    claimed = Some(g.tracked_take());
                }
            }
        );
        if was_taken {
            return None;
        }
        let tracked mut perm = claimed.tracked_unwrap();
        let consumers = self.consumers.take(Tracked(&mut perm));
        Some((consumers, Tracked(perm)))
    }

    /// Drop everything the channel still owns: the consumer halves, if
    /// unclaimed (their own `Drop` drops the items left in the rings), and
    /// every producer half nobody registered. Run when the last handle goes
    /// away; without it, those halves would stay in their `PCell`s, which
    /// never drop their contents, and leak.
    #[verifier::exec_allows_no_decreases_clause]
    fn drain(&self)
        requires
            self.wf(),
    {
        let _consumers = self.claim_consumers();
        loop
            invariant
                self.wf(),
        {
            match self.register() {
                Some(_producer) => {},
                None => break,
            }
        }
    }
}

/// A multi-producer, single-consumer channel: one SPSC ring per producer.
/// Handles can be cloned and shared across threads, like the production
/// `Channel`.
pub struct Channel<T> {
    shared: Arc<ChannelShared<T>>,
}

impl<T> Channel<T> {
    pub closed spec fn wf(&self) -> bool {
        self.shared.wf()
    }

    /// The rings, by producer id.
    pub closed spec fn rings(&self) -> Seq<InstanceId> {
        self.shared.rings@
    }

    /// `max_producers` rings of `2^ring_bits` slots each.
    pub fn new(ring_bits: u8, max_producers: usize) -> (r: Self)
        requires
            1 <= ring_bits <= MAX_RING_BITS,
        ensures
            r.wf(),
            r.rings().len() == max_producers,
    {
        let mut producers: Vec<PCell<Producer<T>>> = Vec::new();
        let mut consumers: Vec<Consumer<T>> = Vec::new();
        let tracked mut perms: Map<nat, PointsTo<Producer<T>>> = Map::tracked_empty();
        let ghost mut rings: Seq<InstanceId> = Seq::empty();
        let mut i: usize = 0;
        while i < max_producers
            invariant
                1 <= ring_bits <= MAX_RING_BITS,
                i <= max_producers,
                producers@.len() == i,
                rings.len() == i,
                forall |s: nat| #[trigger] perms.dom().contains(s) <==> s < i,
                forall |s: nat| s < i ==> {
                    &&& #[trigger] perms[s].id() == producers@[s as int].id()
                    &&& perms[s].is_init()
                    &&& perms[s].value().wf()
                    &&& perms[s].value().id() == rings[s as int]
                },
                consumers_ok(consumers@, rings),
            decreases max_producers - i,
        {
            let (p, c) = spsc_channel::<T>(ring_bits);
            proof {
                rings = rings.push(p.id());
            }
            let (cell, Tracked(perm)) = PCell::new(p);
            producers.push(cell);
            proof {
                perms.tracked_insert(i as nat, perm);
            }
            consumers.push(c);
            i = i + 1;
        }
        let ghost cell_ids = producers@.map_values(|c: PCell<Producer<T>>| c.id());
        proof {
            assert forall |s: nat| s < max_producers implies #[trigger] perms[s].id() == cell_ids[s as int] by {
                assert(cell_ids[s as int] == producers@[s as int].id());
            }
        }
        let (consumers_cell, Tracked(consumers_perm)) = PCell::new(consumers);
        let producer_count = AtomicU64::new(Ghost((cell_ids, rings)), 0, Tracked(perms));
        let consumer_taken = AtomicBool::new(
            Ghost((consumers_cell.id(), rings)),
            false,
            Tracked(Some(consumers_perm)),
        );
        Channel {
            shared: Arc::new(ChannelShared {
                producers,
                consumers: consumers_cell,
                producer_count,
                consumer_taken,
                n: max_producers,
                rings: Ghost(rings),
            }),
        }
    }

    /// Another handle to the same channel (`Channel` is `Clone` in the
    /// production crate).
    pub fn clone_handle(&self) -> (r: Self)
        requires
            self.wf(),
        ensures
            r.wf(),
            r.rings() == self.rings(),
    {
        Channel { shared: arc_clone(&self.shared) }
    }

    /// Register a producer: the next unclaimed ring's producer half, or
    /// `None` once all `max_producers` are taken.
    ///
    /// Retries its compare-and-swap if another thread registers first, so
    /// it can spin under contention; it never hands out a ring twice.
    pub fn register(&self) -> (r: Option<(usize, Producer<T>)>)
        requires
            self.wf(),
        ensures
            r matches Some((id, p)) ==> {
                &&& id < self.rings().len()
                &&& p.wf()
                &&& p.id() == self.rings()[id as int]
            },
    {
        self.shared.register()
    }

    /// Claim the consumer: every ring's consumer half, or `None` while
    /// another `ChannelConsumer` exists. Dropping it releases the claim.
    // The claim's permission is used only by ghost code, which plain builds
    // erase.
    #[allow(unused_variables)]
    pub fn consumer(&self) -> (r: Option<ChannelConsumer<T>>)
        requires
            self.wf(),
        ensures
            r matches Some(cc) ==> cc.wf() && cc.rings() == self.rings(),
    {
        match self.shared.claim_consumers() {
            Some((consumers, perm)) => {
                let tracked perm = perm.get();
                Some(ChannelConsumer {
                    shared: arc_clone(&self.shared),
                    consumers,
                    perm: Tracked(Some(perm)),
                })
            },
            None => None,
        }
    }
}

/// The consuming end of a `Channel`: every ring's consumer half. At most one
/// exists at a time.
pub struct ChannelConsumer<T> {
    shared: Arc<ChannelShared<T>>,
    consumers: Vec<Consumer<T>>,
    // Ghost: the consumers cell's permission, given back on release.
    #[allow(dead_code)]
    perm: Tracked<Option<PointsTo<Vec<Consumer<T>>>>>,
}

impl<T> ChannelConsumer<T> {
    pub closed spec fn wf(&self) -> bool {
        &&& self.shared.wf()
        &&& self.perm@ is Some
        &&& self.perm@->Some_0.id() == self.shared.consumers.id()
        &&& self.perm@->Some_0.is_uninit()
        &&& consumers_ok(self.consumers@, self.shared.rings@)
    }

    pub closed spec fn rings(&self) -> Seq<InstanceId> {
        self.shared.rings@
    }

    /// Everything received from producer `i`'s ring, in order.
    pub closed spec fn consumed(&self, i: int) -> Seq<T> {
        self.consumers@[i].consumed()
    }

    /// The consumer half of ring `i`, for stating per-producer FIFO with
    /// `spsc::fifo_holds`.
    pub fn ring(&self, i: usize) -> (c: &Consumer<T>)
        requires
            self.wf(),
            i < self.rings().len(),
        ensures
            c.wf(),
            c.id() == self.rings()[i as int],
            c.consumed() == self.consumed(i as int),
    {
        proof {
            assert(self.consumers@[i as int].wf());
        }
        &self.consumers[i]
    }

    /// `consume_all_up_to_owned`: up to `max_per_ring` items from each ring,
    /// ring 0 first. Each ring's items come out in the order its producer
    /// sent them (INV-CH-03).
    pub fn consume_all(&mut self, max_per_ring: usize) -> (out: Vec<T>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            final(self).rings() == old(self).rings(),
            forall |i: int| 0 <= i < old(self).rings().len() ==>
                is_prefix(old(self).consumed(i), #[trigger] final(self).consumed(i)),
    {
        let mut out: Vec<T> = Vec::new();
        let mut i: usize = 0;
        let n = self.consumers.len();
        while i < n
            invariant
                self.wf(),
                self.rings() == old(self).rings(),
                n == self.consumers@.len(),
                i <= n,
                forall |j: int| 0 <= j < n ==>
                    is_prefix(old(self).consumed(j), #[trigger] self.consumed(j)),
            decreases n - i,
        {
            let ghost before = self.consumers@;
            proof {
                // The loop invariant, about the consumers before this call.
                assert forall |j: int| 0 <= j < n implies
                    is_prefix(old(self).consumed(j), #[trigger] before[j].consumed()) by {
                    assert(self.consumed(j) == before[j].consumed());
                }
            }
            let mut batch = self.consumers[i].consume_up_to(max_per_ring);
            proof {
                // Only ring `i` changed, and it grew by exactly `batch`.
                assert(self.consumers@ == before.update(i as int, self.consumers@[i as int]));
                assert forall |j: int| 0 <= j < n implies {
                    &&& #[trigger] self.consumers@[j].wf()
                    &&& self.consumers@[j].id() == self.shared.rings@[j]
                } by {
                    if j != i as int {
                        assert(self.consumers@[j] == before[j]);
                    }
                }
                assert forall |j: int| 0 <= j < n implies
                    is_prefix(old(self).consumed(j), #[trigger] self.consumed(j)) by {
                    let a = old(self).consumed(j);
                    let b = before[j].consumed();
                    assert(is_prefix(a, b));
                    if j == i as int {
                        let c = self.consumed(j);
                        assert(c == b + batch@);
                        assert forall |k: int| 0 <= k < a.len() implies #[trigger] a[k] == c[k] by {
                            assert(a[k] == b[k]);
                            assert(c[k] == b[k]);
                        }
                    } else {
                        assert(self.consumers@[j] == before[j]);
                    }
                }
            }
            out.append(&mut batch);
            i = i + 1;
        }
        out
    }

    /// Give the consumer halves back and release the claim.
    fn release(&mut self)
        requires
            old(self).wf(),
    {
        let mut consumers: Vec<Consumer<T>> = Vec::new();
        core::mem::swap(&mut consumers, &mut self.consumers);
        let tracked mut perm = self.perm.borrow_mut().tracked_take();
        self.shared.consumers.put(Tracked(&mut perm), consumers);
        atomic_with_ghost!(&self.shared.consumer_taken => store(false);
            ghost g => {
                if g is Some {
                    // Two permissions for one cell can't exist, so the flag
                    // was `true` and `g` was empty.
                    perm.is_exclusive(g.tracked_borrow());
                }
                g = Some(perm);
            }
        );
    }
}

} // verus!

/// Dropping the `ChannelConsumer` releases the claim, as the production
/// `Consumer` does.
///
/// Trusted glue, outside `verus!`: it calls the verified `release`, whose
/// precondition `wf()` holds for every `ChannelConsumer` safe code can
/// obtain. Its fields are private, `Channel::consumer` establishes `wf()`,
/// and every method preserves it.
impl<T> Drop for ChannelConsumer<T> {
    fn drop(&mut self) {
        self.release();
    }
}

/// When the last handle goes away, drop what the channel still owns:
/// unregistered producer halves and unclaimed consumer halves (and so the
/// items left in their rings). Without this they would leak.
///
/// Trusted glue, outside `verus!`: it calls the verified `drain`, whose
/// precondition `wf()` holds for every `ChannelShared`: its fields are
/// private, only `Channel::new` builds one, and nothing mutates it except
/// through its atomics. A `ChannelConsumer` holds an `Arc` to it, so this
/// can't run while a consumer is claimed.
impl<T> Drop for ChannelShared<T> {
    fn drop(&mut self) {
        self.drain();
    }
}
