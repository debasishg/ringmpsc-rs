//! The SPSC ring protocol as a VerusSync state machine.
//!
//! This is the Verus counterpart of `crates/ringmpsc/tla/RingSPSC.qnt`. Quint
//! model-checks that model for `CAPACITY = 4, MAX_ITEMS = 8`. Here the
//! invariants are proven inductive, so they hold for every capacity and every
//! number of items.
//!
//! The two models agree on state, actions and the safety invariants,
//! including cache soundness (`cachesLag` in Quint, `caches_lag` here) and
//! a producer guarded by `cached_head`. One thing the Quint model abstracts
//! away is modeled here:
//!
//! - **Batches and partial commits.** `reserve(n)` claims `n` slots,
//!   `commit(k)` publishes the first `k <= n` of them (`try_commit_n`,
//!   `commit_up_to`), and `abandon()` drops a reservation without committing
//!   (`Reservation` has no `Drop` impl). The consumer reads a whole window
//!   `[head, end)` and may advance by fewer than it read (`consume_up_to`).
//!
//! The model covers the protocol only: who may touch which sequence numbers
//! when. Memory orderings are out of scope (Verus treats atomics as
//! sequentially consistent), as are slot contents, which come with the exec
//! ring.

// `tokenized_state_machine!` names its generated module after the machine.
#![allow(non_snake_case)]

use vstd::prelude::*;
use verus_state_machines_macros::tokenized_state_machine;

verus! {

/// What the producer is doing.
pub ghost enum ProducerState {
    Idle,
    /// Holding a reservation for sequence numbers `[tail, tail + n)`.
    Reserved { n: nat },
}

/// What the consumer is doing.
pub ghost enum ConsumerState {
    Idle,
    /// Reading sequence numbers `[head, end)`, before it stores the new head.
    Reading { end: nat },
}

/// Two sequence numbers less than `cap` apart never share a slot.
///
/// The ring's slot for `seq` is `seq % cap` (`arith::slot_index`), so this is
/// the arithmetic behind every "these slots don't overlap" argument.
pub proof fn lemma_close_seqs_distinct_slots(a: nat, b: nat, cap: nat)
    requires
        cap > 0,
        a < b < a + cap,
    ensures
        a % cap != b % cap,
{
    let (ai, bi, ci) = (a as int, b as int, cap as int);
    vstd::arithmetic::div_mod::lemma_fundamental_div_mod(ai, ci);
    vstd::arithmetic::div_mod::lemma_fundamental_div_mod(bi, ci);
    vstd::arithmetic::div_mod::lemma_mod_bound(ai, ci);
    vstd::arithmetic::div_mod::lemma_mod_bound(bi, ci);
    if ai % ci == bi % ci {
        // Then b - a = cap * (b / cap - a / cap), a nonzero multiple of cap,
        // which cannot be smaller than cap.
        assert(bi - ai == ci * (bi / ci - ai / ci)) by (nonlinear_arith)
            requires
                ai == ci * (ai / ci) + ai % ci,
                bi == ci * (bi / ci) + bi % ci,
                ai % ci == bi % ci;
        let q = bi / ci - ai / ci;
        assert(q >= 1) by (nonlinear_arith)
            requires bi - ai == ci * q, bi - ai > 0, ci > 0;
        assert(bi - ai >= ci) by (nonlinear_arith)
            requires bi - ai == ci * q, q >= 1, ci > 0;
    }
}

} // verus!

tokenized_state_machine!{
    RingSpsc {
        fields {
            #[sharding(constant)]
            pub cap: nat,

            /// Next sequence number the consumer will read. Written by the
            /// consumer only (`head.store(Release)`).
            #[sharding(variable)]
            pub head: nat,

            /// Next sequence number the producer will write. Written by the
            /// producer only (`tail.store(Release)`).
            #[sharding(variable)]
            pub tail: nat,

            /// The producer's last observation of `head`.
            #[sharding(variable)]
            pub cached_head: nat,

            /// The consumer's last observation of `tail`.
            #[sharding(variable)]
            pub cached_tail: nat,

            #[sharding(variable)]
            pub producer: ProducerState,

            #[sharding(variable)]
            pub consumer: ConsumerState,
        }

        // ---------------------------------------------------------------
        // Invariants
        // ---------------------------------------------------------------

        /// INV-SEQ-01 (bounded count) and INV-ORD-03 (head never passes tail).
        #[invariant]
        pub fn bounded_count(&self) -> bool {
            &&& self.cap > 0
            &&& self.head <= self.tail
            &&& self.tail <= self.head + self.cap
        }

        /// The caches lag the real values. Each side only moves its own
        /// cursor forward, so whatever the other side last saw is still a
        /// lower bound.
        #[invariant]
        pub fn caches_lag(&self) -> bool {
            &&& self.cached_head <= self.head
            &&& self.cached_tail <= self.tail
        }

        /// INV-INIT-03: a reservation covers unpublished slots only, and all
        /// of them fit in the ring without reaching the consumer's slots.
        #[invariant]
        pub fn reservation_in_free_space(&self) -> bool {
            self.producer matches ProducerState::Reserved { n } ==> {
                &&& n > 0
                &&& self.tail + n <= self.head + self.cap
            }
        }

        /// INV-INIT-01: the consumer reads published slots only.
        #[invariant]
        pub fn read_within_published(&self) -> bool {
            self.consumer matches ConsumerState::Reading { end } ==> {
                &&& self.head < end
                &&& end <= self.tail
            }
        }

        // ---------------------------------------------------------------
        // Initial state
        // ---------------------------------------------------------------

        init!{
            initialize(cap: nat) {
                require cap > 0;
                init cap = cap;
                init head = 0;
                init tail = 0;
                init cached_head = 0;
                init cached_tail = 0;
                init producer = ProducerState::Idle;
                init consumer = ConsumerState::Idle;
            }
        }

        // ---------------------------------------------------------------
        // Producer
        // ---------------------------------------------------------------

        /// `reserve_shared`: claim `[tail, tail + n)` if `cached_head` says
        /// there is room. This covers both paths: the fast path uses the
        /// cache as is, and the slow path runs `refresh_head` first.
        transition!{
            reserve(n: nat) {
                require pre.producer is Idle;
                require n > 0;
                require pre.tail + n <= pre.cached_head + pre.cap;
                update producer = ProducerState::Reserved { n };
            }
        }

        /// Slow path of `reserve_shared`: `head.load(Acquire)` into the cache.
        transition!{
            refresh_head() {
                update cached_head = pre.head;
            }
        }

        /// `commit_internal`: publish the first `k` reserved slots with
        /// `tail.store(Release)`. `commit()` passes `k = n`; `try_commit_n`
        /// and `commit_up_to` may pass less.
        transition!{
            commit(k: nat) {
                require pre.producer matches ProducerState::Reserved { n } && 0 < k && k <= n;
                update tail = pre.tail + k;
                update producer = ProducerState::Idle;
            }
        }

        /// A `Reservation` dropped without committing: nothing is published.
        transition!{
            abandon() {
                require pre.producer is Reserved;
                update producer = ProducerState::Idle;
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

        /// `consume_batch*` / `consume_up_to*`: load `tail` directly
        /// (Acquire) and read `[head, end)` for some `end` it saw.
        transition!{
            read_fresh(end: nat) {
                require pre.consumer is Idle;
                require pre.head < end && end <= pre.tail;
                update consumer = ConsumerState::Reading { end };
            }
        }

        /// Fast path of `readable`: read `[head, end)` using only
        /// `cached_tail`. The guard needs `head < cached_tail`, which is what
        /// the code tests since #17 (see `arith::readable_avail`). Before, it
        /// tested `cached_tail.wrapping_sub(head) != 0`, which differs when
        /// `cached_tail < head`.
        transition!{
            read_cached(end: nat) {
                require pre.consumer is Idle;
                require pre.head < end && end <= pre.cached_tail;
                update consumer = ConsumerState::Reading { end };
            }
        }

        /// `head.store(Release)` at the end of a consume, or `advance(k)`:
        /// release the first `k` slots read.
        transition!{
            advance(k: nat) {
                require pre.consumer matches ConsumerState::Reading { end } && pre.head + k <= end;
                update head = pre.head + k;
                update consumer = ConsumerState::Idle;
            }
        }

        // ---------------------------------------------------------------
        // Properties: what the invariants buy
        // ---------------------------------------------------------------

        /// INV-INIT-03, as the code needs it: while the producer writes slot
        /// `seq % cap` for some reserved `seq`, the consumer is not reading
        /// that slot. This is the property behind every `SAFETY` comment in
        /// `make_reservation` and `consume_batch`.
        property!{
            producer_and_consumer_slots_disjoint(write_seq: nat, read_seq: nat) {
                require pre.producer matches ProducerState::Reserved { n }
                    && pre.tail <= write_seq && write_seq < pre.tail + n;
                require pre.consumer matches ConsumerState::Reading { end }
                    && pre.head <= read_seq && read_seq < end;
                assert(write_seq % pre.cap != read_seq % pre.cap) by {
                    lemma_close_seqs_distinct_slots(read_seq, write_seq, pre.cap);
                };
            }
        }

        /// The `&mut [MaybeUninit<T>]` a reservation hands out has no two
        /// elements on the same slot.
        property!{
            reserved_slots_distinct(s1: nat, s2: nat) {
                require pre.producer matches ProducerState::Reserved { n }
                    && pre.tail <= s1 && s1 < s2 && s2 < pre.tail + n;
                assert(s1 % pre.cap != s2 % pre.cap) by {
                    lemma_close_seqs_distinct_slots(s1, s2, pre.cap);
                };
            }
        }

        /// INV-INIT-01: anything the consumer reads was published.
        property!{
            reads_are_published(read_seq: nat) {
                require pre.consumer matches ConsumerState::Reading { end }
                    && pre.head <= read_seq && read_seq < end;
                assert(read_seq < pre.tail);
            }
        }

        // ---------------------------------------------------------------
        // Inductiveness proofs
        // ---------------------------------------------------------------

        #[inductive(initialize)]
        fn initialize_inductive(post: Self, cap: nat) { }

        #[inductive(reserve)]
        fn reserve_inductive(pre: Self, post: Self, n: nat) { }

        #[inductive(refresh_head)]
        fn refresh_head_inductive(pre: Self, post: Self) { }

        #[inductive(commit)]
        fn commit_inductive(pre: Self, post: Self, k: nat) { }

        #[inductive(abandon)]
        fn abandon_inductive(pre: Self, post: Self) { }

        #[inductive(refresh_tail)]
        fn refresh_tail_inductive(pre: Self, post: Self) { }

        #[inductive(read_fresh)]
        fn read_fresh_inductive(pre: Self, post: Self, end: nat) { }

        #[inductive(read_cached)]
        fn read_cached_inductive(pre: Self, post: Self, end: nat) { }

        #[inductive(advance)]
        fn advance_inductive(pre: Self, post: Self, k: nat) { }
    }
}
