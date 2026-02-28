// Quint Formal Specification for SPSC Ring Buffer
// Translated from RingSPSC.tla
//
// This module models the lock-free single-producer single-consumer ring buffer
// implemented in ringmpsc-rs. It verifies the core invariants from spec.md:
//
//   - INV-SEQ-01: Bounded Count (0 ≤ tl - hd ≤ Capacity)
//   - INV-SEQ-02: Monotonic Progress (hd/tl only increase)
//   - INV-ORD-03: Happens-Before Chain (hd ≤ tl)
//   - INV-SW-01:  Producer-Owned Fields (tl, cached_head)
//   - INV-SW-02:  Consumer-Owned Fields (hd, cached_tail)
//
// Run with:
//   quint run RingSPSC.qnt --main=RingSPSC --max-steps=100
//   quint run RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant   (simulation + invariant checking)
//   quint test RingSPSC.qnt --main=RingSPSC                             (run embedded tests)
//   quint verify RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant --backend=tlc  (exhaustive model checking)
//
// Since Quint 0.31.0 the Rust backend is the default for `quint run` and `quint test`,
// giving ~10× faster simulation than the TypeScript backend. TLC is now available as an
// alternative backend for `quint verify` via `--backend=tlc`, enabling exhaustive model
// checking directly from this .qnt file without maintaining a separate .tla file.
//
// Note: `head`/`tail` are Quint built-in names (list ops), so we use `hd`/`tl`.

module RingSPSC {
    // =========================================================================
    // CONSTANTS
    // =========================================================================
    
    /// Ring buffer capacity (power of 2 in Rust impl, any positive here)
    val CAPACITY = 4
    
    /// Maximum items to produce (bounds state space for model checking)
    val MAX_ITEMS = 8

    // =========================================================================
    // STATE VARIABLES  
    // =========================================================================
    
    /// Consumer's read position (only consumer writes) - INV-SW-02
    var hd: int
    
    /// Producer's write position (only producer writes) - INV-SW-01
    var tl: int
    
    /// Producer's cached view of hd (optimization modeling)
    var cached_head: int
    
    /// Consumer's cached view of tl (optimization modeling)
    var cached_tail: int
    
    /// Total items produced (for termination bound)
    var items_produced: int

    // =========================================================================
    // PURE FUNCTIONS
    // =========================================================================
    
    /// Current number of items in ring
    pure def count(t: int, h: int): int = t - h
    
    /// Check if producer has space (fast path using cached_head)
    pure def producerHasSpace(t: int, ch: int): bool = (t - ch) < CAPACITY
    
    /// Check if consumer has items (fast path using cached_tail)
    pure def consumerHasItems(ct: int, h: int): bool = ct > h

    // =========================================================================
    // INVARIANTS - from spec.md
    // =========================================================================
    
    /// INV-SEQ-01: Bounded Count
    /// "0 ≤ (tl - hd) ≤ capacity"
    val boundedCount: bool = 
        tl >= hd and (tl - hd) <= CAPACITY
    
    /// INV-ORD-03: Happens-Before (consumer never reads ahead of producer)
    val happensBefore: bool = hd <= tl
    
    /// Combined safety invariant
    val safetyInvariant: bool = boundedCount and happensBefore

    // =========================================================================
    // INITIAL STATE
    // =========================================================================
    
    action init = all {
        hd' = 0,
        tl' = 0,
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
        producerHasSpace(tl, cached_head),
        items_produced < MAX_ITEMS,
        // No state change - reservation returned but not committed
        hd' = hd,
        tl' = tl,
        cached_head' = cached_head,
        cached_tail' = cached_tail,
        items_produced' = items_produced,
    }
    
    /// ProducerRefreshCache: Reload hd when cache is stale
    /// Rust: ring.rs reserve() slow path - load head with Acquire
    /// Models INV-ORD-01 Acquire semantics
    action producerRefreshCache = all {
        not(producerHasSpace(tl, cached_head)),  // Fast path failed
        cached_head < hd,                         // Cache is stale
        cached_head' = hd,                        // Refresh cache
        // Unchanged
        hd' = hd,
        tl' = tl,
        cached_tail' = cached_tail,
        items_produced' = items_produced,
    }
    
    /// ProducerWrite: Commit item to ring
    /// Rust: ring.rs commit_internal() - store tail with Release
    /// Models INV-ORD-01 Release semantics (publishes writes)
    action producerWrite = all {
        (tl - hd) < CAPACITY,                      // Actual space check
        items_produced < MAX_ITEMS,                 // Bound for model checking
        tl' = tl + 1,                              // Release store
        items_produced' = items_produced + 1,
        // Unchanged
        hd' = hd,
        cached_head' = cached_head,
        cached_tail' = cached_tail,
    }

    // =========================================================================
    // CONSUMER ACTIONS
    // =========================================================================
    
    /// ConsumerReadFast: Check cached_tail for items (no state change)
    /// Rust: ring.rs consume_batch() fast path
    action consumerReadFast = all {
        consumerHasItems(cached_tail, hd),
        // No state change - just checking
        hd' = hd,
        tl' = tl,
        cached_head' = cached_head,
        cached_tail' = cached_tail,
        items_produced' = items_produced,
    }
    
    /// ConsumerRefreshCache: Reload tl when cache is stale
    /// Rust: ring.rs consume_batch() slow path - load tail with Acquire
    /// Models INV-ORD-02 Acquire semantics (synchronizes with producer's Release)
    action consumerRefreshCache = all {
        not(consumerHasItems(cached_tail, hd)),     // Fast path failed
        cached_tail < tl,                            // Cache is stale
        cached_tail' = tl,                           // Refresh cache
        // Unchanged
        hd' = hd,
        tl' = tl,
        cached_head' = cached_head,
        items_produced' = items_produced,
    }
    
    /// ConsumerAdvance: Consume item from ring
    /// Rust: ring.rs advance() / end of consume_batch()
    /// Models INV-ORD-02 Release semantics on head
    action consumerAdvance = all {
        hd < tl,                                    // Items available
        hd' = hd + 1,                               // Release store
        // Unchanged
        tl' = tl,
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
        hd < tl,                                  // Consumer can still read
        items_produced == MAX_ITEMS and hd == tl,  // Terminated normally
    }

    // =========================================================================
    // TESTS (for quint test)
    // =========================================================================

    /// Stutter step: no state change (used in assertion-only test steps)
    action stutter = all {
        hd' = hd,
        tl' = tl,
        cached_head' = cached_head,
        cached_tail' = cached_tail,
        items_produced' = items_produced,
    }
    
    /// Test: Initial state satisfies invariants
    run initSatisfiesInvariant = {
        init.then(all {
            assert(safetyInvariant),
            assert(boundedCount),
            assert(happensBefore),
            stutter,
        })
    }
    
    /// Test: Producer write maintains bounded count
    run producerWriteMaintainsBoundedCount = {
        init
        .then(producerWrite)
        .then(all {
            assert(boundedCount),
            assert(tl == 1),
            assert(items_produced == 1),
            stutter,
        })
    }
    
    /// Test: Consumer advance maintains happens-before
    run consumerAdvanceMaintainsHappensBefore = {
        init
        .then(producerWrite)
        .then(consumerAdvance)
        .then(all {
            assert(happensBefore),
            assert(hd == 1),
            assert(tl == 1),
            stutter,
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
            assert(tl - hd == CAPACITY),
            stutter,
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
        // Producer's cached_head is still 0, actual hd is 2
        // producerHasSpace(4, 0) = false, but producerHasSpace(4, 2) = true
        .then(producerRefreshCache)
        .then(all {
            assert(cached_head == 2),
            assert(producerHasSpace(tl, cached_head)),
            stutter,
        })
    }
}
