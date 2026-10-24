//! Slot ownership: the SPSC protocol with the ring's memory in it.
//!
//! `protocol.rs` proves *which* sequence numbers each side may touch. This
//! machine adds the memory: every slot is a `PCell<T>`, and its `PointsTo`
//! permission token lives in the `slots` storage map. A side that wants to
//! touch a slot has to withdraw that slot's token through a transition, and
//! the transition only allows it when the protocol says so. Since a
//! `PointsTo` can't be duplicated, two sides can never hold the same slot.
//!
//! Tokens are keyed by **sequence number**, over the window
//! `[head, head + cap)`. Sequence number `s` uses cell `cells[s % cap]`. When
//! the consumer releases a slot, its token goes back under `s + cap`, the
//! next sequence number that will use the same cell.
//!
//! The ring's contents are tracked as two ghost histories: `produced`, owned
//! by the producer, and `consumed`, owned by the consumer. The invariant
//! `fifo` says `consumed` is always a prefix of `produced`: the consumer gets
//! exactly what the producer sent, in order. Splitting the history this way
//! (one shared queue would be updated by both sides) is what lets each side
//! run on its own thread holding only its own tokens (#23).
//!
//! **Batches** (#24): the producer reserves `n` slots at once and publishes
//! the first `k <= n` of them (`commit_up_to`; `k = 0` abandons the
//! reservation). The consumer holds the `n` published slots it is reading and
//! releases the first `k` of them. A single push or pop is the `n = 1` case.

// `tokenized_state_machine!` names its generated module after the machine.
#![allow(non_snake_case)]

use vstd::cell::pcell_maybe_uninit::PointsTo;
use vstd::cell::CellId;
use vstd::prelude::*;
use verus_state_machines_macros::tokenized_state_machine;

verus! {

/// `(s + cap) % cap == s % cap`: a released slot's token goes back under
/// `s + cap`, and still belongs to the same cell.
pub proof fn lemma_next_lap_same_cell(s: nat, cap: nat)
    requires
        cap > 0,
    ensures
        (s + cap) % cap == s % cap,
{
    vstd::arithmetic::div_mod::lemma_mod_add_multiples_vanish(s as int, cap as int);
}

/// `a` is a prefix of `b`.
pub open spec fn is_prefix<T>(a: Seq<T>, b: Seq<T>) -> bool {
    &&& a.len() <= b.len()
    &&& forall |i: int| 0 <= i < a.len() ==> #[trigger] a[i] == b[i]
}

/// The sequence numbers `[lo, hi)`.
pub open spec fn range(lo: nat, hi: nat) -> Set<nat> {
    Set::<nat>::range(lo, hi)
}

// Membership in `Set::range`: `lo <= s < hi`.
broadcast use vstd::set_lib::range_set_properties;

/// What a consumer that held `[head, head + held)` and consumed the first
/// `k` deposits back: the unconsumed slots under their own numbers, and the
/// consumed ones under `s + cap`, their cells' next sequence numbers.
pub open spec fn release_keys(head: nat, held: nat, k: nat, cap: nat) -> Set<nat> {
    range(head + k, head + held).union(range(head + cap, head + cap + k))
}

} // verus!

tokenized_state_machine!{
    SpscSlots<T> {
        fields {
            #[sharding(constant)]
            pub cap: nat,

            /// The cell each slot index lives in; `cells.len() == cap`.
            #[sharding(constant)]
            pub cells: Seq<CellId>,

            /// Slot permissions held by the protocol, keyed by sequence number.
            #[sharding(storage_map)]
            pub slots: Map<nat, PointsTo<T>>,

            /// Lives in the `head` atomic. Written by the consumer only.
            #[sharding(variable)]
            pub head: nat,

            /// Lives in the `tail` atomic. Written by the producer only.
            #[sharding(variable)]
            pub tail: nat,

            /// Producer-owned.
            #[sharding(variable)]
            pub cached_head: nat,

            /// Consumer-owned.
            #[sharding(variable)]
            pub cached_tail: nat,

            /// Producer-owned: it holds the tokens for `[tail, tail + reserved)`.
            #[sharding(variable)]
            pub reserved: nat,

            /// Consumer-owned: it holds the tokens for `[head, head + held)`.
            #[sharding(variable)]
            pub held: nat,

            /// Consumer-owned: the items in the slots it holds.
            #[sharding(variable)]
            pub held_values: Seq<T>,

            /// Producer-owned: every item ever published, in order.
            #[sharding(variable)]
            pub produced: Seq<T>,

            /// Consumer-owned: every item ever consumed, in order.
            #[sharding(variable)]
            pub consumed: Seq<T>,
        }

        // ---------------------------------------------------------------
        // Invariants
        // ---------------------------------------------------------------

        /// INV-SEQ-01, INV-ORD-03, and both caches lag. The histories'
        /// lengths are the cursors.
        #[invariant]
        pub fn cursors(&self) -> bool {
            &&& self.cap > 0
            &&& self.cells.len() == self.cap
            &&& self.head <= self.tail
            &&& self.tail <= self.head + self.cap
            &&& self.cached_head <= self.head
            &&& self.cached_tail <= self.tail
            &&& self.produced.len() == self.tail
            &&& self.consumed.len() == self.head
        }

        /// The producer only holds free slots; the consumer only published ones.
        #[invariant]
        pub fn holders(&self) -> bool {
            &&& self.tail + self.reserved <= self.head + self.cap
            &&& self.head + self.held <= self.tail
            &&& self.held_values.len() == self.held
        }

        /// FIFO: the consumer got exactly what the producer sent, in order,
        /// and what it holds now is what comes next.
        #[invariant]
        pub fn fifo(&self) -> bool {
            &&& is_prefix(self.consumed, self.produced)
            &&& forall |i: int| 0 <= i < self.held ==>
                    #[trigger] self.held_values[i] == self.produced[self.head + i]
        }

        /// Every sequence number in the window has its token either in
        /// storage or with the side that withdrew it, never both.
        #[invariant]
        pub fn window(&self) -> bool {
            &&& forall |s: nat| #[trigger] self.slots.dom().contains(s) ==>
                    self.head <= s < self.head + self.cap && !self.withdrawn(s)
            &&& forall |s: nat| self.head <= s < self.head + self.cap && !self.withdrawn(s) ==>
                    #[trigger] self.slot_ok(s)
        }

        /// The slot for `s` is held by the producer or the consumer.
        pub open spec fn withdrawn(&self, s: nat) -> bool {
            ||| (self.tail <= s < self.tail + self.reserved)
            ||| (self.head <= s < self.head + self.held)
        }

        /// INV-INIT-01/02 for one sequence number: its token is in storage,
        /// belongs to its cell, and is initialized with the produced item iff
        /// `s` was published.
        pub open spec fn slot_ok(&self, s: nat) -> bool {
            &&& self.slots.dom().contains(s)
            &&& self.slots[s].id() == self.cells[(s % self.cap) as int]
            &&& s < self.tail ==> {
                &&& self.slots[s].is_init()
                &&& self.slots[s].value() == self.produced[s as int]
            }
            &&& s >= self.tail ==> self.slots[s].is_uninit()
        }

        // ---------------------------------------------------------------
        // Initial state
        // ---------------------------------------------------------------

        init!{
            initialize(cap: nat, cells: Seq<CellId>, perms: Map<nat, PointsTo<T>>) {
                require cap > 0;
                require cells.len() == cap;
                require forall |s: nat| #[trigger] perms.dom().contains(s) <==> s < cap;
                require forall |s: nat| s < cap ==> {
                    &&& #[trigger] perms[s].id() == cells[s as int]
                    &&& perms[s].is_uninit()
                };
                init cap = cap;
                init cells = cells;
                init slots = perms;
                init head = 0;
                init tail = 0;
                init cached_head = 0;
                init cached_tail = 0;
                init reserved = 0;
                init held = 0;
                init held_values = Seq::empty();
                init produced = Seq::empty();
                init consumed = Seq::empty();
            }
        }

        // ---------------------------------------------------------------
        // Producer
        // ---------------------------------------------------------------

        /// Slow path of `reserve`: `head.load(Acquire)` into the cache.
        transition!{
            refresh_head() {
                update cached_head = pre.head;
            }
        }

        /// `reserve(n)`: take the free slots `[tail, tail + n)` (`tail` is
        /// `produced.len()`, which the producer owns), if the cache says
        /// there is room.
        transition!{
            produce_start(n: nat) {
                require pre.reserved == 0;
                require n > 0;
                require pre.produced.len() + n <= pre.cached_head + pre.cap;
                update reserved = n;
                birds_eye let lo = pre.produced.len();
                birds_eye let m = pre.slots.restrict(range(lo, lo + n));
                withdraw slots -= (m) by {
                    assert forall |s: nat| #[trigger] m.dom().contains(s) implies
                        pre.slots.dom().contains(s) && pre.slots[s] == m[s] by {
                        assert(pre.slot_ok(s));
                    }
                };
                assert(m.dom() == range(lo, lo + n)) by {
                    assert forall |s: nat| #[trigger] m.dom().contains(s) <==> range(lo, lo + n).contains(s) by {
                        if lo <= s < lo + n {
                            assert(!pre.withdrawn(s));
                            assert(pre.slot_ok(s));
                        }
                    }
                    assert(m.dom() =~= range(lo, lo + n));
                };
                assert(forall |s: nat| lo <= s < lo + n ==>
                    #[trigger] m[s].id() == pre.cells[(s % pre.cap) as int]) by {
                    assert forall |s: nat| lo <= s < lo + n implies
                        #[trigger] m[s].id() == pre.cells[(s % pre.cap) as int] by {
                        assert(!pre.withdrawn(s));
                        assert(pre.slot_ok(s));
                        assert(m.dom().contains(s));
                        assert(m[s] == pre.slots[s]);
                    }
                };
                assert(forall |s: nat| lo <= s < lo + n ==> #[trigger] m[s].is_uninit()) by {
                    assert forall |s: nat| lo <= s < lo + n implies #[trigger] m[s].is_uninit() by {
                        assert(!pre.withdrawn(s));
                        assert(pre.slot_ok(s));
                        assert(m.dom().contains(s));
                        assert(m[s] == pre.slots[s]);
                    }
                };
            }
        }

        /// `commit_up_to(k)`: publish the first `k` reserved slots with
        /// `tail.store(tail + k, Release)`, and return all of them. `k = 0`
        /// abandons the reservation.
        transition!{
            produce_end(k: nat, perms: Map<nat, PointsTo<T>>) {
                require pre.reserved > 0;
                require k <= pre.reserved;
                require perms.dom() == range(pre.tail, pre.tail + pre.reserved);
                require forall |s: nat| pre.tail <= s < pre.tail + pre.reserved ==>
                    #[trigger] perms[s].id() == pre.cells[(s % pre.cap) as int];
                require forall |s: nat| pre.tail <= s < pre.tail + k ==> #[trigger] perms[s].is_init();
                require forall |s: nat| pre.tail + k <= s < pre.tail + pre.reserved ==> #[trigger] perms[s].is_uninit();
                update reserved = 0;
                update tail = pre.tail + k;
                update produced = pre.produced + Seq::new(k, |i: int| perms[(pre.tail + i) as nat].value());
                deposit slots += (perms) by {
                    assert forall |s: nat| #[trigger] perms.dom().contains(s) implies !pre.slots.dom().contains(s) by {}
                };
            }
        }

        // ---------------------------------------------------------------
        // Consumer
        // ---------------------------------------------------------------

        /// Slow path of `readable`: `tail.load(Acquire)` into the cache.
        transition!{
            refresh_tail() {
                update cached_tail = pre.tail;
            }
        }

        /// `readable`: take the published slots `[head, head + n)` (`head` is
        /// `consumed.len()`, which the consumer owns), if the cache says they
        /// were published. The guard is `head + n <= cached_tail`, so
        /// `head < cached_tail` as in `readable` since #17.
        transition!{
            consume_start(n: nat) {
                require pre.held == 0;
                require n > 0;
                require pre.consumed.len() + n <= pre.cached_tail;
                // The consumer never holds more than the ring has slots, so the
                // ranges it deposits back in `consume_end` don't overlap.
                assert(n <= pre.cap);
                update held = n;
                birds_eye let lo = pre.consumed.len();
                birds_eye let m = pre.slots.restrict(range(lo, lo + n));
                withdraw slots -= (m) by {
                    assert forall |s: nat| #[trigger] m.dom().contains(s) implies
                        pre.slots.dom().contains(s) && pre.slots[s] == m[s] by {
                        assert(pre.slot_ok(s));
                    }
                };
                update held_values = Seq::new(n, |i: int| m[(lo + i) as nat].value());
                assert(m.dom() == range(lo, lo + n)) by {
                    assert forall |s: nat| #[trigger] m.dom().contains(s) <==> range(lo, lo + n).contains(s) by {
                        if lo <= s < lo + n {
                            assert(!pre.withdrawn(s));
                            assert(pre.slot_ok(s));
                        }
                    }
                    assert(m.dom() =~= range(lo, lo + n));
                };
                assert(forall |s: nat| lo <= s < lo + n ==>
                    #[trigger] m[s].id() == pre.cells[(s % pre.cap) as int]) by {
                    assert forall |s: nat| lo <= s < lo + n implies
                        #[trigger] m[s].id() == pre.cells[(s % pre.cap) as int] by {
                        assert(!pre.withdrawn(s));
                        assert(pre.slot_ok(s));
                        assert(m.dom().contains(s));
                        assert(m[s] == pre.slots[s]);
                    }
                };
                assert(forall |s: nat| lo <= s < lo + n ==> #[trigger] m[s].is_init()) by {
                    assert forall |s: nat| lo <= s < lo + n implies #[trigger] m[s].is_init() by {
                        assert(!pre.withdrawn(s));
                        assert(pre.slot_ok(s));
                        assert(m.dom().contains(s));
                        assert(m[s] == pre.slots[s]);
                    }
                };
            }
        }

        /// `release(k)`: consume the first `k` held slots with
        /// `head.store(head + k, Release)`. They come back empty, under
        /// `s + cap`; the rest come back untouched, under their own numbers.
        transition!{
            consume_end(k: nat, perms: Map<nat, PointsTo<T>>) {
                require pre.held > 0;
                require k <= pre.held;
                require perms.dom() == release_keys(pre.head, pre.held, k, pre.cap);
                require forall |s: nat| pre.head + k <= s < pre.head + pre.held ==> {
                    &&& #[trigger] perms[s].id() == pre.cells[(s % pre.cap) as int]
                    &&& perms[s].is_init()
                    &&& perms[s].value() == pre.held_values[s - pre.head]
                };
                require forall |s: nat| pre.head + pre.cap <= s < pre.head + pre.cap + k ==> {
                    &&& #[trigger] perms[s].id() == pre.cells[(s % pre.cap) as int]
                    &&& perms[s].is_uninit()
                };
                update held = 0;
                update head = pre.head + k;
                update consumed = pre.consumed + pre.held_values.take(k as int);
                update held_values = Seq::empty();
                deposit slots += (perms) by {
                    assert forall |s: nat| #[trigger] perms.dom().contains(s) implies !pre.slots.dom().contains(s) by {}
                };
            }
        }

        // ---------------------------------------------------------------
        // Properties: facts a token holder can learn
        // ---------------------------------------------------------------

        /// FIFO, for whoever holds both histories (for example after joining
        /// the producer and consumer threads).
        property!{
            fifo_holds() {
                assert(is_prefix(pre.consumed, pre.produced));
            }
        }

        /// The producer's own tokens: its cache is never ahead of `tail`.
        property!{
            producer_facts() {
                assert(pre.cached_head <= pre.produced.len());
            }
        }

        /// Inside the `tail` atomic: the published `tail` is the producer's
        /// history length.
        property!{
            tail_is_produced_len() {
                assert(pre.tail == pre.produced.len());
            }
        }

        /// Inside the `head` atomic: the published `head` is the consumer's
        /// history length.
        property!{
            head_is_consumed_len() {
                assert(pre.head == pre.consumed.len());
            }
        }

        /// Inside the `head` atomic, for the producer: what it reserved is
        /// still free space. (Used to show `tail + k` can't overflow.)
        property!{
            reserved_in_window() {
                assert(pre.produced.len() + pre.reserved <= pre.head + pre.cap);
            }
        }

        /// Everything at once, for the single-threaded ring that holds every
        /// token.
        property!{
            cursor_facts() {
                assert({
                    &&& pre.head <= pre.tail
                    &&& pre.tail <= pre.head + pre.cap
                    &&& pre.cached_head <= pre.head
                    &&& pre.cached_tail <= pre.tail
                    &&& pre.produced.len() == pre.tail
                    &&& pre.consumed.len() == pre.head
                    &&& pre.head + pre.held <= pre.tail
                    &&& pre.tail + pre.reserved <= pre.head + pre.cap
                    &&& is_prefix(pre.consumed, pre.produced)
                    &&& forall |i: int| 0 <= i < pre.held ==>
                            #[trigger] pre.held_values[i] == pre.produced[pre.head + i]
                });
            }
        }

        // ---------------------------------------------------------------
        // Inductiveness proofs
        // ---------------------------------------------------------------

        #[inductive(initialize)]
        fn initialize_inductive(post: Self, cap: nat, cells: Seq<CellId>, perms: Map<nat, PointsTo<T>>) {
            assert forall |s: nat| post.head <= s < post.head + post.cap && !post.withdrawn(s)
                implies #[trigger] post.slot_ok(s) by {
                vstd::arithmetic::div_mod::lemma_small_mod(s, cap);
            }
        }

        #[inductive(refresh_head)]
        fn refresh_head_inductive(pre: Self, post: Self) {
            assert forall |s: nat| post.head <= s < post.head + post.cap && !post.withdrawn(s)
                implies #[trigger] post.slot_ok(s) by {
                assert(pre.slot_ok(s));
            }
        }

        #[inductive(produce_start)]
        fn produce_start_inductive(pre: Self, post: Self, n: nat) {
            assert forall |s: nat| post.head <= s < post.head + post.cap && !post.withdrawn(s)
                implies #[trigger] post.slot_ok(s) by {
                assert(pre.slot_ok(s));
            }
        }

        #[inductive(produce_end)]
        fn produce_end_inductive(pre: Self, post: Self, k: nat, perms: Map<nat, PointsTo<T>>) {
            let added = Seq::new(k, |i: int| perms[(pre.tail + i) as nat].value());
            assert forall |s: nat| post.head <= s < post.head + post.cap && !post.withdrawn(s)
                implies #[trigger] post.slot_ok(s) by {
                if pre.tail <= s < pre.tail + pre.reserved {
                    assert(perms.dom().contains(s));
                    if s < post.tail {
                        assert(post.produced[s as int] == added[s - pre.tail]);
                    }
                } else {
                    assert(pre.slot_ok(s));
                    if s < pre.tail {
                        assert(post.produced[s as int] == pre.produced[s as int]);
                    }
                }
            }
            assert forall |i: int| 0 <= i < post.consumed.len()
                implies #[trigger] post.consumed[i] == post.produced[i] by {
                assert(pre.consumed[i] == pre.produced[i]);
            }
            assert forall |i: int| 0 <= i < post.held
                implies #[trigger] post.held_values[i] == post.produced[post.head + i] by {
                assert(pre.held_values[i] == pre.produced[pre.head + i]);
            }
        }

        #[inductive(refresh_tail)]
        fn refresh_tail_inductive(pre: Self, post: Self) {
            assert forall |s: nat| post.head <= s < post.head + post.cap && !post.withdrawn(s)
                implies #[trigger] post.slot_ok(s) by {
                assert(pre.slot_ok(s));
            }
        }

        #[inductive(consume_start)]
        fn consume_start_inductive(pre: Self, post: Self, n: nat) {
            assert forall |s: nat| post.head <= s < post.head + post.cap && !post.withdrawn(s)
                implies #[trigger] post.slot_ok(s) by {
                assert(pre.slot_ok(s));
            }
            assert forall |i: int| 0 <= i < post.held
                implies #[trigger] post.held_values[i] == post.produced[post.head + i] by {
                assert(pre.slot_ok((pre.head + i) as nat));
            }
        }

        #[inductive(consume_end)]
        fn consume_end_inductive(pre: Self, post: Self, k: nat, perms: Map<nat, PointsTo<T>>) {
            assert forall |s: nat| post.head <= s < post.head + post.cap && !post.withdrawn(s)
                implies #[trigger] post.slot_ok(s) by {
                if pre.head + k <= s < pre.head + pre.held {
                    assert(perms.dom().contains(s));
                    assert(pre.held_values[s - pre.head] == pre.produced[s as int]);
                } else if s >= pre.head + pre.cap {
                    assert(perms.dom().contains(s));
                    lemma_next_lap_same_cell((s - pre.cap) as nat, pre.cap);
                } else {
                    assert(pre.slot_ok(s));
                }
            }
            assert forall |i: int| 0 <= i < post.consumed.len()
                implies #[trigger] post.consumed[i] == post.produced[i] by {
                if i < pre.consumed.len() {
                    assert(pre.consumed[i] == pre.produced[i]);
                } else {
                    assert(pre.held_values[i - pre.head] == pre.produced[i]);
                }
            }
        }
    }
}
