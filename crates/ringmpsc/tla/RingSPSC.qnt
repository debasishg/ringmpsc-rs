// Quint Formal Specification for SPSC Ring Buffer
// Translated from RingSPSC.tla
//
// This module models the lock-free single-producer single-consumer ring buffer
// implemented in ringmpsc-rs. It verifies the core invariants from spec.md:
//
//   - INV-SEQ-01: Bounded Count (0 ≤ tail - head ≤ Capacity)
//   - INV-SEQ-02: Monotonic Progress (head/tail only increase)
//   - INV-ORD-03: Happens-Before Chain (head ≤ tail)
//   - INV-SW-01:  Producer-Owned Fields (tail, cached_head)
//   - INV-SW-02:  Consumer-Owned Fields (head, cached_tail)
//
// Run with: quint run RingSPSC.qnt --main=RingSPSC --max-steps=100

module RingSPSC {
    // =========================================================================
    // CONSTANTS
    // =========================================================================
    
    /// Ring buffer capacity (power of 2 in Rust impl, any positive here)
    const CAPACITY: int = 4
    
    /// Maximum items to produce (bounds state space for model checking)
    const MAX_ITEMS: int = 8

    // =========================================================================
    // STATE VARIABLES  
    // =========================================================================
    
    /// Consumer's read position (only consumer writes) - INV-SW-02
    var head: int
    
    /// Producer's write position (only producer writes) - INV-SW-01
    var tail: int
    
    /// Producer's cached view of head (optimization modeling)
    var cached_head: int
    
    /// Consumer's cached view of tail (optimization modeling)
    var cached_tail: int
    
    /// Total items produced (for termination bound)
    var items_produced: int

    // =========================================================================
    // PURE FUNCTIONS
    // =========================================================================
    
    /// Current number of items in ring
    pure def count(): int = tail - head
    
    /// Check if producer has space (fast path using cached_head)
    pure def producerHasSpace(t: int, ch: int): bool = (t - ch) < CAPACITY
    
    /// Check if consumer has items (fast path using cached_tail)
    pure def consumerHasItems(ct: int, h: int): bool = ct > h

    // =========================================================================
    // INVARIANTS - from spec.md
    // =========================================================================
    
    /// INV-SEQ-01: Bounded Count
    /// "0 ≤ (tail - head) ≤ capacity"
    val boundedCount: bool = 
        tail >= head and (tail - head) <= CAPACITY
    
    /// INV-ORD-03: Happens-Before (consumer never reads ahead of producer)
    val happensBefore: bool = head <= tail
    
    /// Combined safety invariant
    val safetyInvariant: bool = boundedCount and happensBefore

    // =========================================================================
    // INITIAL STATE
    // =========================================================================
    
    action init = all {
        head' = 0,
        tail' = 0,
        cached_head' = 0,
        cached_tail' = 0,
        items_produced' = 0,
    }

    // =========================================================================
    // PRODUCER ACTIONS
    // =========================================================================
    
    /// ProducerReserveFast: Check cached_head for space (no state change)
    /// Rust: ring.rs reserve() fast path
    action producerReserveFast = all {
        producerHasSpace(tail, cached_head),
        items_produced < MAX_ITEMS,
        // No state change - reservation returned but not committed
        head' = head,
        tail' = tail,
        cached_head' = cached_head,
        cached_tail' = cached_tail,
        items_produced' = items_produced,
    }
    
    /// ProducerRefreshCache: Reload head when cache is stale
    /// Rust: ring.rs reserve() slow path - load head with Acquire
    /// Models INV-ORD-01 Acquire semantics
    action producerRefreshCache = all {
        not(producerHasSpace(tail, cached_head)),  // Fast path failed
        cached_head < head,                         // Cache is stale
        cached_head' = head,                        // Refresh cache
        // Unchanged
        head' = head,
        tail' = tail,
        cached_tail' = cached_tail,
        items_produced' = items_produced,
    }
    
    /// ProducerWrite: Commit item to ring
    /// Rust: ring.rs commit_internal() - store tail with Release
    /// Models INV-ORD-01 Release semantics (publishes writes)
    action producerWrite = all {
        (tail - head) < CAPACITY,                   // Actual space check
        items_produced < MAX_ITEMS,                 // Bound for model checking
        tail' = tail + 1,                           // Release store
        items_produced' = items_produced + 1,
        // Unchanged
        head' = head,
        cached_head' = cached_head,
        cached_tail' = cached_tail,
    }

    // =========================================================================
    // CONSUMER ACTIONS
    // =========================================================================
    
    /// ConsumerReadFast: Check cached_tail for items (no state change)
    /// Rust: ring.rs consume_batch() fast path
    action consumerReadFast = all {
        consumerHasItems(cached_tail, head),
        // No state change - just checking
        head' = head,
        tail' = tail,
        cached_head' = cached_head,
        cached_tail' = cached_tail,
        items_produced' = items_produced,
    }
    
    /// ConsumerRefreshCache: Reload tail when cache is stale
    /// Rust: ring.rs consume_batch() slow path - load tail with Acquire
    /// Models INV-ORD-02 Acquire semantics (synchronizes with producer's Release)
    action consumerRefreshCache = all {
        not(consumerHasItems(cached_tail, head)),   // Fast path failed
        cached_tail < tail,                          // Cache is stale
        cached_tail' = tail,                         // Refresh cache
        // Unchanged
        head' = head,
        tail' = tail,
        cached_head' = cached_head,
        items_produced' = items_produced,
    }
    
    /// ConsumerAdvance: Consume item from ring
    /// Rust: ring.rs advance() / end of consume_batch()
    /// Models INV-ORD-02 Release semantics on head
    action consumerAdvance = all {
        head < tail,                                // Items available
        head' = head + 1,                           // Release store
        // Unchanged
        tail' = tail,
        cached_head' = cached_head,
        cached_tail' = cached_tail,
        items_produced' = items_produced,
    }

    // =========================================================================
    // SPECIFICATION
    // =========================================================================
    
    /// All possible next states
    action step = any {
        producerReserveFast,
        producerRefreshCache,
        producerWrite,
        consumerReadFast,
        consumerRefreshCache,
        consumerAdvance,
    }

    // =========================================================================
    // TEMPORAL PROPERTIES
    // =========================================================================
    
    /// No deadlock: some action is always enabled (or system terminated)
    val noDeadlock: bool = or {
        items_produced < MAX_ITEMS,              // Producer can still write
        head < tail,                              // Consumer can still read
        items_produced == MAX_ITEMS and head == tail,  // Terminated normally
    }

    // =========================================================================
    // TESTS (for quint test)
    // =========================================================================
    
    /// Test: Initial state satisfies invariants
    run initSatisfiesInvariant = {
        init.then(all {
            assert(safetyInvariant),
            assert(boundedCount),
            assert(happensBefore),
        })
    }
    
    /// Test: Producer write maintains bounded count
    run producerWriteMaintainsBoundedCount = {
        init
        .then(producerWrite)
        .then(all {
            assert(boundedCount),
            assert(tail == 1),
            assert(items_produced == 1),
        })
    }
    
    /// Test: Consumer advance maintains happens-before
    run consumerAdvanceMaintainsHappensBefore = {
        init
        .then(producerWrite)
        .then(consumerAdvance)
        .then(all {
            assert(happensBefore),
            assert(head == 1),
            assert(tail == 1),
        })
    }
    
    /// Test: Fill ring to capacity
    run fillToCapacity = {
        init
        .then(producerWrite)
        .then(producerWrite)
        .then(producerWrite)
        .then(producerWrite)
        .then(all {
            assert(boundedCount),
            assert(tail - head == CAPACITY),
            // Cannot write more (ring full)
        })
    }

    /// Test: Cache refresh scenario
    run cacheRefreshScenario = {
        init
        // Fill ring
        .then(producerWrite)
        .then(producerWrite)
        .then(producerWrite)
        .then(producerWrite)
        // Consumer advances but producer cache is stale
        .then(consumerAdvance)
        .then(consumerAdvance)
        // Producer's cached_head is still 0, actual head is 2
        // producerHasSpace(4, 0) = false, but producerHasSpace(4, 2) = true
        .then(producerRefreshCache)
        .then(all {
            assert(cached_head == 2),
            assert(producerHasSpace(tail, cached_head)),
        })
    }
}
