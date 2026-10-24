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
//! The machine also carries `queue`, the ghost contents of the ring, so the
//! exec ring can be proven a correct FIFO queue and not just a memory-safe
//! one.
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

            #[sharding(variable)]
            pub head: nat,

            #[sharding(variable)]
            pub tail: nat,

            #[sharding(variable)]
            pub cached_head: nat,

            #[sharding(variable)]
            pub cached_tail: nat,

            /// The producer holds the token for `tail`.
            #[sharding(variable)]
            pub producing: bool,

            /// The consumer holds the token for `head`.
            #[sharding(variable)]
            pub consuming: bool,

            /// Ghost contents of the ring: `queue[i]` is the item at `head + i`.
            #[sharding(variable)]
            pub queue: Seq<T>,
        }

        // ---------------------------------------------------------------
        // Invariants
        // ---------------------------------------------------------------

        /// INV-SEQ-01, INV-ORD-03, and both caches lag.
        #[invariant]
        pub fn cursors(&self) -> bool {
            &&& self.cap > 0
            &&& self.cells.len() == self.cap
            &&& self.head <= self.tail
            &&& self.tail <= self.head + self.cap
            &&& self.cached_head <= self.head
            &&& self.cached_tail <= self.tail
            &&& self.queue.len() == self.tail - self.head
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
        /// belongs to its cell, and is initialized with the queued item iff
        /// `s` was published.
        pub open spec fn slot_ok(&self, s: nat) -> bool {
            &&& self.slots.dom().contains(s)
            &&& self.slots[s].id() == self.cells[(s % self.cap) as int]
            &&& s < self.tail ==> {
                &&& self.slots[s].is_init()
                &&& self.slots[s].value() == self.queue[s - self.head]
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
                init queue = Seq::empty();
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

        /// Take the free slot for `tail`, if the cache says there is room.
        transition!{
            produce_start() {
                require !pre.producing;
                require pre.tail < pre.cached_head + pre.cap;
                update producing = true;
                withdraw slots -= [pre.tail => let perm] by {
                    assert(pre.slot_ok(pre.tail));
                };
                assert({
                    &&& perm.id() == pre.cells[(pre.tail % pre.cap) as int]
                    &&& perm.is_uninit()
                }) by {
                    assert(pre.slot_ok(pre.tail));
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
                update queue = pre.queue.push(perm.value());
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

        /// Take the full slot for `head`, if the cache says it was published.
        /// The guard is `head < cached_tail`, as in `readable` since #17.
        transition!{
            consume_start() {
                require !pre.consuming;
                require pre.head < pre.cached_tail;
                update consuming = true;
                withdraw slots -= [pre.head => let perm] by {
                    assert(pre.slot_ok(pre.head));
                };
                assert({
                    &&& perm.id() == pre.cells[(pre.head % pre.cap) as int]
                    &&& perm.is_init()
                    &&& perm.value() == pre.queue[0]
                }) by {
                    assert(pre.slot_ok(pre.head));
                };
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
                update queue = pre.queue.drop_first();
                deposit slots += [pre.head + pre.cap => perm] by {
                    assert(!pre.slots.dom().contains(pre.head + pre.cap));
                };
            }
        }

        // ---------------------------------------------------------------
        // Properties: facts the exec ring can learn from its tokens
        // ---------------------------------------------------------------

        /// The cursor invariants, exported for the exec ring. It holds every
        /// token, but invariants are only visible through properties.
        property!{
            cursor_facts() {
                assert({
                    &&& pre.head <= pre.tail
                    &&& pre.tail <= pre.head + pre.cap
                    &&& pre.cached_head <= pre.head
                    &&& pre.cached_tail <= pre.tail
                    &&& pre.queue.len() == pre.tail - pre.head
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
            assert forall |s: nat| post.head <= s < post.head + post.cap && !post.held(s)
                implies #[trigger] post.slot_ok(s) by {
                assert(pre.slot_ok(s));
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
