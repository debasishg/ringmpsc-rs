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
//! the consumer releases `head`, the token goes back under `head + cap`, the
//! next sequence number that will use the same cell.
//!
//! The ring's contents are tracked as two ghost histories: `produced`, owned
//! by the producer, and `consumed`, owned by the consumer. The invariant
//! `fifo` says `consumed` is always a prefix of `produced`: the consumer gets
//! exactly what the producer sent, in order. Splitting the history this way
//! (one shared queue would be updated by both sides) is what lets each side
//! run on its own thread holding only its own tokens (#23).
//!
//! Each side's transitions need only tokens that side owns, except the
//! cursor it publishes (`tail` for the producer, `head` for the consumer) and
//! the one it refreshes from. Those tokens live inside the `head` and `tail`
//! atomics, so the exec code touches them only within an atomic load or
//! store.
//!
//! This is the single-slot form: one item per produce or consume. Batches
//! (`reserve(n)`, `commit(k)`) come with the zero-copy `Reservation` (#24).

// `tokenized_state_machine!` names its generated module after the machine.
#![allow(non_snake_case)]

use vstd::cell::pcell_maybe_uninit::PointsTo;
use vstd::cell::CellId;
use vstd::prelude::*;
use verus_state_machines_macros::tokenized_state_machine;

verus! {

/// `(s + cap) % cap == s % cap`: the token for `head` goes back under
/// `head + cap`, and still belongs to the same cell.
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

            /// Producer-owned: it holds the token for `tail`.
            #[sharding(variable)]
            pub producing: bool,

            /// Consumer-owned: it holds the token for `head`.
            #[sharding(variable)]
            pub consuming: bool,

            /// Producer-owned: every item ever pushed, in order.
            #[sharding(variable)]
            pub produced: Seq<T>,

            /// Consumer-owned: every item ever taken, in order.
            #[sharding(variable)]
            pub consumed: Seq<T>,
        }

        // ---------------------------------------------------------------
        // Invariants
        // ---------------------------------------------------------------

        /// INV-SEQ-01, INV-ORD-03, and both caches lag. The histories'
        /// lengths are the cursors (the consumer's runs one ahead while it
        /// holds a slot).
        #[invariant]
        pub fn cursors(&self) -> bool {
            &&& self.cap > 0
            &&& self.cells.len() == self.cap
            &&& self.head <= self.tail
            &&& self.tail <= self.head + self.cap
            &&& self.cached_head <= self.head
            &&& self.cached_tail <= self.tail
            &&& self.produced.len() == self.tail
            &&& self.consumed.len() == if self.consuming { self.head + 1 } else { self.head }
        }

        /// FIFO: the consumer got exactly what the producer sent, in order.
        #[invariant]
        pub fn fifo(&self) -> bool {
            is_prefix(self.consumed, self.produced)
        }

        /// The producer only holds a free slot; the consumer only a full one.
        #[invariant]
        pub fn holders(&self) -> bool {
            &&& self.producing ==> self.tail < self.head + self.cap
            &&& self.consuming ==> self.head < self.tail
        }

        /// Every sequence number in the window has its token either in
        /// storage or with the side that withdrew it, never both.
        #[invariant]
        pub fn window(&self) -> bool {
            &&& forall |s: nat| #[trigger] self.slots.dom().contains(s) ==>
                    self.head <= s < self.head + self.cap && !self.held(s)
            &&& forall |s: nat| self.head <= s < self.head + self.cap && !self.held(s) ==>
                    #[trigger] self.slot_ok(s)
        }

        /// The slot for `s` is withdrawn by the producer or the consumer.
        pub open spec fn held(&self, s: nat) -> bool {
            ||| (self.producing && s == self.tail)
            ||| (self.consuming && s == self.head)
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
                init producing = false;
                init consuming = false;
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

        /// Take the free slot for `tail` (`produced.len()`, which the
        /// producer owns), if the cache says there is room.
        transition!{
            produce_start() {
                require !pre.producing;
                require pre.produced.len() < pre.cached_head + pre.cap;
                update producing = true;
                withdraw slots -= [pre.produced.len() => let perm] by {
                    assert(pre.slot_ok(pre.produced.len()));
                };
                assert({
                    &&& perm.id() == pre.cells[(pre.produced.len() % pre.cap) as int]
                    &&& perm.is_uninit()
                }) by {
                    assert(pre.slot_ok(pre.produced.len()));
                };
            }
        }

        /// Publish the written slot: `tail.store(tail + 1, Release)`.
        transition!{
            produce_end(perm: PointsTo<T>) {
                require pre.producing;
                require perm.id() == pre.cells[(pre.tail % pre.cap) as int];
                require perm.is_init();
                update producing = false;
                update tail = pre.tail + 1;
                update produced = pre.produced.push(perm.value());
                deposit slots += [pre.tail => perm];
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

        /// Take the full slot for `head` (`consumed.len()`, which the
        /// consumer owns), if the cache says it was published. The guard is
        /// `head < cached_tail`, as in `readable` since #17. The item joins
        /// the consumer's history.
        transition!{
            consume_start() {
                require !pre.consuming;
                require pre.consumed.len() < pre.cached_tail;
                update consuming = true;
                withdraw slots -= [pre.consumed.len() => let perm] by {
                    assert(pre.slot_ok(pre.consumed.len()));
                };
                assert({
                    &&& perm.id() == pre.cells[(pre.consumed.len() % pre.cap) as int]
                    &&& perm.is_init()
                }) by {
                    assert(pre.slot_ok(pre.consumed.len()));
                };
                update consumed = pre.consumed.push(perm.value());
            }
        }

        /// Release the emptied slot: `head.store(head + 1, Release)`. Its
        /// token goes back under `head + cap`, the slot's next sequence number.
        transition!{
            consume_end(perm: PointsTo<T>) {
                require pre.consuming;
                require perm.id() == pre.cells[(pre.head % pre.cap) as int];
                require perm.is_uninit();
                update consuming = false;
                update head = pre.head + 1;
                deposit slots += [pre.head + pre.cap => perm] by {
                    assert(!pre.slots.dom().contains(pre.head + pre.cap));
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
        /// history length (one less while it holds a slot).
        property!{
            head_is_consumed_len() {
                assert(pre.consumed.len() == if pre.consuming { pre.head + 1 } else { pre.head });
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
                    &&& pre.consumed.len() == if pre.consuming { pre.head + 1 } else { pre.head }
                    &&& is_prefix(pre.consumed, pre.produced)
                });
            }
        }

        // ---------------------------------------------------------------
        // Inductiveness proofs
        // ---------------------------------------------------------------

        #[inductive(initialize)]
        fn initialize_inductive(post: Self, cap: nat, cells: Seq<CellId>, perms: Map<nat, PointsTo<T>>) {
            assert forall |s: nat| post.head <= s < post.head + post.cap && !post.held(s)
                implies #[trigger] post.slot_ok(s) by {
                vstd::arithmetic::div_mod::lemma_small_mod(s, cap);
            }
        }

        #[inductive(refresh_head)]
        fn refresh_head_inductive(pre: Self, post: Self) {
            assert forall |s: nat| post.head <= s < post.head + post.cap && !post.held(s)
                implies #[trigger] post.slot_ok(s) by {
                assert(pre.slot_ok(s));
            }
        }

        #[inductive(produce_start)]
        fn produce_start_inductive(pre: Self, post: Self) {
            assert forall |s: nat| post.head <= s < post.head + post.cap && !post.held(s)
                implies #[trigger] post.slot_ok(s) by {
                assert(pre.slot_ok(s));
            }
        }

        #[inductive(produce_end)]
        fn produce_end_inductive(pre: Self, post: Self, perm: PointsTo<T>) {
            assert forall |s: nat| post.head <= s < post.head + post.cap && !post.held(s)
                implies #[trigger] post.slot_ok(s) by {
                if s != pre.tail {
                    assert(pre.slot_ok(s));
                }
            }
            assert forall |i: int| 0 <= i < post.consumed.len()
                implies #[trigger] post.consumed[i] == post.produced[i] by {
                assert(pre.consumed[i] == pre.produced[i]);
            }
        }

        #[inductive(refresh_tail)]
        fn refresh_tail_inductive(pre: Self, post: Self) {
            assert forall |s: nat| post.head <= s < post.head + post.cap && !post.held(s)
                implies #[trigger] post.slot_ok(s) by {
                assert(pre.slot_ok(s));
            }
        }

        #[inductive(consume_start)]
        fn consume_start_inductive(pre: Self, post: Self) {
            assert(pre.slot_ok(pre.head));
            assert forall |s: nat| post.head <= s < post.head + post.cap && !post.held(s)
                implies #[trigger] post.slot_ok(s) by {
                assert(pre.slot_ok(s));
            }
            assert forall |i: int| 0 <= i < post.consumed.len()
                implies #[trigger] post.consumed[i] == post.produced[i] by {
                if i < pre.consumed.len() {
                    assert(pre.consumed[i] == pre.produced[i]);
                }
            }
        }

        #[inductive(consume_end)]
        fn consume_end_inductive(pre: Self, post: Self, perm: PointsTo<T>) {
            lemma_next_lap_same_cell(pre.head, pre.cap);
            assert forall |s: nat| post.head <= s < post.head + post.cap && !post.held(s)
                implies #[trigger] post.slot_ok(s) by {
                if s != pre.head + pre.cap {
                    assert(pre.slot_ok(s));
                }
            }
        }
    }
}
