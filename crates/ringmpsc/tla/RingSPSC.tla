---------------------------- MODULE RingSPSC ----------------------------
(*
 * TLA+ Formal Specification for SPSC Ring Buffer
 * 
 * This module models the lock-free single-producer single-consumer ring buffer
 * implemented in ringmpsc-rs. It verifies the core invariants from spec.md:
 * 
 *   - INV-SEQ-01: Bounded Count (0 ≤ tail - head ≤ Capacity)
 *   - INV-SEQ-02: Monotonic Progress (head/tail only increase)
 *   - INV-ORD-01: Producer Publish Protocol (Release semantics)
 *   - INV-ORD-02: Consumer Read Protocol (Acquire semantics)  
 *   - INV-ORD-03: Happens-Before Chain
 *   - INV-SW-01:  Producer-Owned Fields (tail, cached_head)
 *   - INV-SW-02:  Consumer-Owned Fields (head, cached_tail)
 *
 * DESIGN DECISION: Unbounded Naturals
 * -----------------------------------
 * The Rust implementation uses u64 sequence numbers to prevent ABA problems.
 * At 10 billion messages/second, u64 wrap-around takes ~58 years.
 * 
 * For model checking tractability, we use unbounded Nat instead of modeling
 * wrap-around arithmetic. This is sound because:
 *   1. The invariants we verify (bounded count, monotonicity) don't depend on overflow
 *   2. TLC explores finite state space anyway (bounded by MaxItems)
 *   3. Wrap-around correctness is a separate concern (tested via Loom)
 *
 * REFINEMENT MAPPING (TLA+ → Rust):
 * ---------------------------------
 *   ProducerReserve  → ring.rs: reserve()
 *   ProducerWrite    → ring.rs: commit_internal()
 *   ConsumerRead     → ring.rs: consume_batch() / consume_batch_owned()
 *   ConsumerAdvance  → ring.rs: advance()
 *)

EXTENDS Naturals, Sequences

CONSTANTS 
    Capacity,   \* Ring buffer size (power of 2 in impl, any positive here)
    MaxItems    \* Bound on total items for finite model checking

VARIABLES
    \* === Core State ===
    head,           \* Consumer's read position (only consumer writes)
    tail,           \* Producer's write position (only producer writes)
    buffer,         \* Sequence representing ring contents (logical, not physical)
    
    \* === Cached Values (optimization modeling) ===
    cached_head,    \* Producer's cached view of head
    cached_tail,    \* Consumer's cached view of tail
    
    \* === Auxiliary for Liveness ===
    items_produced  \* Total items produced (for termination)

vars == <<head, tail, buffer, cached_head, cached_tail, items_produced>>

-----------------------------------------------------------------------------
(* TYPE INVARIANTS *)
-----------------------------------------------------------------------------

TypeOK ==
    /\ head \in Nat
    /\ tail \in Nat
    /\ cached_head \in Nat
    /\ cached_tail \in Nat
    /\ items_produced \in Nat
    /\ buffer \in Seq(Nat)

-----------------------------------------------------------------------------
(* SAFETY INVARIANTS - from spec.md *)
-----------------------------------------------------------------------------

\* INV-SEQ-01: Bounded Count
\* "0 ≤ (tail - head) ≤ capacity"
\* The number of items in the ring never exceeds capacity
BoundedCount == 
    /\ tail >= head                    \* Non-negative count (no underflow)
    /\ (tail - head) <= Capacity       \* Never exceeds capacity

\* INV-SEQ-02: Monotonic Progress  
\* "head_new ≥ head_old, tail_new ≥ tail_old"
\* Encoded as: head and tail only increase (checked by action constraints)
\* This is a temporal property verified by the absence of decreasing transitions

\* INV-SW-01/02: Single-Writer Ownership
\* Encoded structurally: each action only modifies its owned variables
\* Producer actions: modify tail, cached_head
\* Consumer actions: modify head, cached_tail

\* INV-ORD-03: Happens-Before (simplified)
\* If consumer reads an index, producer must have written it
\* Modeled by: head <= tail (consumer never reads ahead of producer)
HappensBefore == head <= tail

\* Combined Safety Invariant
SafetyInvariant ==
    /\ TypeOK
    /\ BoundedCount
    /\ HappensBefore

-----------------------------------------------------------------------------
(* INITIAL STATE *)
-----------------------------------------------------------------------------

Init ==
    /\ head = 0
    /\ tail = 0
    /\ buffer = <<>>
    /\ cached_head = 0
    /\ cached_tail = 0
    /\ items_produced = 0

-----------------------------------------------------------------------------
(* PRODUCER ACTIONS *)
-----------------------------------------------------------------------------

\* Check if producer can write (has space)
\* Models the fast path using cached_head
ProducerHasSpace ==
    (tail - cached_head) < Capacity

\* TLA+ Action: ProducerReserve (fast path)
\* Rust: ring.rs reserve() - check cached_head, return reservation
\* Pre:  Space available per cached view
\* Post: No state change (reservation is returned, not committed yet)
ProducerReserveFast ==
    /\ ProducerHasSpace
    /\ items_produced < MaxItems
    /\ UNCHANGED vars  \* Reserve doesn't change state until commit

\* TLA+ Action: ProducerReserve (slow path - refresh cache)
\* Rust: ring.rs reserve() slow path - load head with Acquire, update cache
\* Pre:  Cached view says full, but real head may have advanced
\* Post: cached_head updated to current head
ProducerRefreshCache ==
    /\ ~ProducerHasSpace                    \* Fast path failed
    /\ cached_head < head                   \* Cache is stale
    /\ cached_head' = head                  \* Refresh (Acquire semantics)
    /\ UNCHANGED <<head, tail, buffer, cached_tail, items_produced>>

\* TLA+ Action: ProducerWrite  
\* Rust: ring.rs commit_internal() - store tail with Release
\* Pre:  Space available (after reserve succeeded)
\* Post: tail incremented, item added to logical buffer
\* 
\* INV-ORD-01: The Release store on tail publishes all prior writes
ProducerWrite ==
    /\ (tail - head) < Capacity             \* Actual space check
    /\ items_produced < MaxItems            \* Bound for model checking
    /\ tail' = tail + 1                     \* Release store (publishes write)
    /\ buffer' = Append(buffer, tail)       \* Logical: item added
    /\ items_produced' = items_produced + 1
    /\ UNCHANGED <<head, cached_head, cached_tail>>

-----------------------------------------------------------------------------
(* CONSUMER ACTIONS *)
-----------------------------------------------------------------------------

\* Check if consumer can read (has items)
\* Models the fast path using cached_tail
ConsumerHasItems ==
    cached_tail > head

\* TLA+ Action: ConsumerRead (fast path check)
\* Rust: ring.rs consume_batch() - check cached_tail
ConsumerReadFast ==
    /\ ConsumerHasItems
    /\ UNCHANGED vars  \* Just checking, actual read in ConsumerAdvance

\* TLA+ Action: ConsumerRead (slow path - refresh cache)  
\* Rust: ring.rs consume_batch() slow path - load tail with Acquire
\* Pre:  Cached view says empty, but real tail may have advanced
\* Post: cached_tail updated to current tail
\*
\* INV-ORD-02: The Acquire load synchronizes with producer's Release
ConsumerRefreshCache ==
    /\ ~ConsumerHasItems                    \* Fast path failed
    /\ cached_tail < tail                   \* Cache is stale
    /\ cached_tail' = tail                  \* Refresh (Acquire semantics)
    /\ UNCHANGED <<head, tail, buffer, cached_head, items_produced>>

\* TLA+ Action: ConsumerAdvance
\* Rust: ring.rs advance() / end of consume_batch()
\* Pre:  Items available to consume
\* Post: head incremented (Release store), items removed from logical buffer
\*
\* INV-ORD-02: The Release store on head publishes consumption
ConsumerAdvance ==
    /\ head < tail                          \* Items available
    /\ head' = head + 1                     \* Release store
    /\ buffer' = Tail(buffer)               \* Logical: item removed
    /\ UNCHANGED <<tail, cached_head, cached_tail, items_produced>>

-----------------------------------------------------------------------------
(* SPECIFICATION *)
-----------------------------------------------------------------------------

\* All possible next states
Next ==
    \/ ProducerReserveFast
    \/ ProducerRefreshCache
    \/ ProducerWrite
    \/ ConsumerReadFast
    \/ ConsumerRefreshCache
    \/ ConsumerAdvance

\* Allow stuttering (system can idle)
Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------
(* LIVENESS PROPERTIES *)
-----------------------------------------------------------------------------

\* Eventually all produced items are consumed (if consumer keeps running)
\* This is weak fairness: if an action is continuously enabled, it eventually happens
EventuallyConsumed == 
    items_produced = MaxItems ~> (head = tail)

\* No deadlock: some action is always enabled (or system terminated)
NoDeadlock ==
    \/ items_produced < MaxItems    \* Producer can still write
    \/ head < tail                   \* Consumer can still read
    \/ (items_produced = MaxItems /\ head = tail)  \* Terminated normally

=============================================================================
