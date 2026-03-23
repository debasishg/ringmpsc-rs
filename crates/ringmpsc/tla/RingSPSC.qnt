// Quint Formal Specification for SPSC Ring Buffer
// Translated from RingSPSC.tla
//
// This module models the lock-free single-producer single-consumer ring buffer
// implemented in ringmpsc-rs. It verifies the core invariants from spec.md:
//
//   - INV-SEQ-01:   Bounded Count (0 ≤ tl - hd ≤ Capacity)
//   - INV-SEQ-02:   Monotonic Progress (hd/tl only increase)
//   - INV-ORD-03:   Happens-Before Chain (hd ≤ tl)
//   - INV-SW-01:    Producer-Owned Fields (tl, cached_head)
//   - INV-SW-02:    Consumer-Owned Fields (hd, cached_tail)
//   - INV-MEM-04:   Allocator Safety Contract (buffer_capacity == CAPACITY)
//   - INV-ALLOC-01: Alignment Guarantee (modeled as buffer_aligned flag)
//   - INV-ALLOC-02: Zero Overhead Default (modeled as allocator_zst flag)
//   - INV-INIT-01:  Initialized Range (tracked via initialized slot set)
//   - INV-NUMA-01:  Memory Placement (modeled as numa_placement_valid flag)
//   - INV-NUMA-02:  Fallback Safety (modeled as numa_fallback_safe flag)
//   - INV-NUMA-03:  Policy Determinism (modeled as numa_policy_deterministic flag)
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
    
    /// Buffer capacity as returned by the allocator (INV-MEM-04)
    /// In a correct system this always equals CAPACITY.
    var buffer_capacity: int
    
    /// Set of buffer indices currently holding initialized data (INV-INIT-01)
    /// Tracks which slots in [0, CAPACITY) contain valid T values.
    var initialized: Set[int]
    
    /// Whether the buffer is aligned (INV-ALLOC-01, structural in Rust)
    /// Modeled as a boolean flag set at construction.
    var buffer_aligned: bool
    
    /// Whether the allocator is a ZST (INV-ALLOC-02, structural in Rust)
    /// Modeled as a boolean flag set at construction.
    var allocator_zst: bool

    /// Whether NUMA memory placement is valid (INV-NUMA-01, runtime on Linux)
    /// On Linux: mbind places pages on the requested node.
    /// Modeled as a boolean flag set at construction.
    var numa_placement_valid: bool

    /// Whether NUMA fallback is safe (INV-NUMA-02, compile-time #[cfg])
    /// Non-Linux platforms fall back to HeapAllocator — no panic, no data loss.
    /// Modeled as a boolean flag set at construction.
    var numa_fallback_safe: bool

    /// Whether NUMA policy is deterministic (INV-NUMA-03)
    /// RoundRobin counter shared via Arc<AtomicU16> across clones.
    /// Modeled as a boolean flag set at construction.
    var numa_policy_deterministic: bool

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
    
    /// INV-MEM-04: Allocator Safety Contract (requirement 1)
    /// allocate(capacity) returns a buffer of exactly CAPACITY elements.
    /// The allocator must provide the correct number of slots for the
    /// ring's hot-path index calculations to be safe.
    val allocatorCapacityCorrect: bool = buffer_capacity == CAPACITY
    
    /// INV-ALLOC-01: Alignment Guarantee
    /// AlignedAllocator<ALIGN> produces buffer pointers aligned to ALIGN bytes.
    /// In the abstract model this is a boolean flag set at construction;
    /// the real check is a debug_assert in AlignedAllocator::allocate().
    val alignmentGuarantee: bool = buffer_aligned
    
    /// INV-ALLOC-02: Zero Overhead Default
    /// HeapAllocator is a zero-sized type, ensuring Ring<T> and
    /// Ring<T, HeapAllocator> have identical layout and codegen.
    /// Structural in Rust (compile-time const assertion); modeled as flag.
    val zeroOverheadDefault: bool = allocator_zst

    /// INV-NUMA-01: Memory Placement
    /// Ring buffer memory is placed on the requested NUMA node.
    /// On Linux verified via /proc/self/numa_maps in debug builds.
    val numaPlacementValid: bool = numa_placement_valid

    /// INV-NUMA-02: Fallback Safety
    /// Non-Linux platforms gracefully fall back to HeapAllocator.
    val numaFallbackSafe: bool = numa_fallback_safe

    /// INV-NUMA-03: Policy Determinism
    /// RoundRobin counter is deterministic per NumaAllocator instance.
    val numaPolicyDeterministic: bool = numa_policy_deterministic
    
    /// INV-INIT-01: Initialized Range
    /// buffer[i] is initialized ⟺ head ≤ sequence(i) < tail
    /// The set of initialized buffer indices must correspond exactly to
    /// the slots whose sequence numbers fall in [hd, tl).
    val initializedRange: bool = {
        val expected = 0.to(tl - hd - 1).map(k => (hd + k) % CAPACITY)
        initialized == expected
    }
    
    /// Combined safety invariant (protocol + allocator + NUMA)
    val safetyInvariant: bool = 
        boundedCount 
        and happensBefore 
        and allocatorCapacityCorrect
        and alignmentGuarantee
        and zeroOverheadDefault
        and initializedRange
        and numaPlacementValid
        and numaFallbackSafe
        and numaPolicyDeterministic

    // =========================================================================
    // INITIAL STATE
    // =========================================================================
    
    action init = all {
        hd' = 0,
        tl' = 0,
        cached_head' = 0,
        cached_tail' = 0,
        items_produced' = 0,
        buffer_capacity' = CAPACITY,
        initialized' = Set(),
        buffer_aligned' = true,
        allocator_zst' = true,
        numa_placement_valid' = true,
        numa_fallback_safe' = true,
        numa_policy_deterministic' = true,
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
        buffer_capacity' = buffer_capacity,
        initialized' = initialized,
        buffer_aligned' = buffer_aligned,
        allocator_zst' = allocator_zst,
        numa_placement_valid' = numa_placement_valid,
        numa_fallback_safe' = numa_fallback_safe,
        numa_policy_deterministic' = numa_policy_deterministic,
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
        buffer_capacity' = buffer_capacity,
        initialized' = initialized,
        buffer_aligned' = buffer_aligned,
        allocator_zst' = allocator_zst,
        numa_placement_valid' = numa_placement_valid,
        numa_fallback_safe' = numa_fallback_safe,
        numa_policy_deterministic' = numa_policy_deterministic,
    }
    
    /// ProducerWrite: Commit item to ring
    /// Rust: ring.rs commit_internal() - store tail with Release
    /// Models INV-ORD-01 Release semantics (publishes writes)
    /// Also models INV-INIT-01: marks the written slot as initialized.
    action producerWrite = all {
        (tl - hd) < CAPACITY,                      // Actual space check
        items_produced < MAX_ITEMS,                 // Bound for model checking
        tl' = tl + 1,                              // Release store
        items_produced' = items_produced + 1,
        initialized' = initialized.union(Set(tl % CAPACITY)),  // INV-INIT-01: slot becomes initialized
        // Unchanged
        hd' = hd,
        cached_head' = cached_head,
        cached_tail' = cached_tail,
        buffer_capacity' = buffer_capacity,
        buffer_aligned' = buffer_aligned,
        allocator_zst' = allocator_zst,
        numa_placement_valid' = numa_placement_valid,
        numa_fallback_safe' = numa_fallback_safe,
        numa_policy_deterministic' = numa_policy_deterministic,
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
        buffer_capacity' = buffer_capacity,
        initialized' = initialized,
        buffer_aligned' = buffer_aligned,
        allocator_zst' = allocator_zst,
        numa_placement_valid' = numa_placement_valid,
        numa_fallback_safe' = numa_fallback_safe,
        numa_policy_deterministic' = numa_policy_deterministic,
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
        buffer_capacity' = buffer_capacity,
        initialized' = initialized,
        buffer_aligned' = buffer_aligned,
        allocator_zst' = allocator_zst,
        numa_placement_valid' = numa_placement_valid,
        numa_fallback_safe' = numa_fallback_safe,
        numa_policy_deterministic' = numa_policy_deterministic,
    }
    
    /// ConsumerAdvance: Consume item from ring
    /// Rust: ring.rs advance() / end of consume_batch()
    /// Models INV-ORD-02 Release semantics on head
    /// Also models INV-INIT-01: marks the consumed slot as uninitialized.
    action consumerAdvance = all {
        hd < tl,                                    // Items available
        hd' = hd + 1,                               // Release store
        initialized' = initialized.exclude(Set(hd % CAPACITY)),  // INV-INIT-01: slot becomes uninitialized
        // Unchanged
        tl' = tl,
        cached_head' = cached_head,
        cached_tail' = cached_tail,
        items_produced' = items_produced,
        buffer_capacity' = buffer_capacity,
        buffer_aligned' = buffer_aligned,
        allocator_zst' = allocator_zst,
        numa_placement_valid' = numa_placement_valid,
        numa_fallback_safe' = numa_fallback_safe,
        numa_policy_deterministic' = numa_policy_deterministic,
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
        buffer_capacity' = buffer_capacity,
        initialized' = initialized,
        buffer_aligned' = buffer_aligned,
        allocator_zst' = allocator_zst,
        numa_placement_valid' = numa_placement_valid,
        numa_fallback_safe' = numa_fallback_safe,
        numa_policy_deterministic' = numa_policy_deterministic,
    }
    
    /// Test: Initial state satisfies invariants
    run initSatisfiesInvariant = {
        init.then(all {
            assert(safetyInvariant),
            assert(boundedCount),
            assert(happensBefore),
            assert(allocatorCapacityCorrect),
            assert(alignmentGuarantee),
            assert(zeroOverheadDefault),
            assert(initializedRange),
            assert(initialized == Set()),
            assert(numaPlacementValid),
            assert(numaFallbackSafe),
            assert(numaPolicyDeterministic),
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
    
    // =====================================================================
    // ALLOCATOR INVARIANT TESTS (INV-MEM-04, INV-ALLOC-01/02, INV-INIT-01)
    // =====================================================================
    
    /// Test: INV-MEM-04 — allocator capacity matches ring capacity at init
    run allocatorCapacityAtInit = {
        init.then(all {
            assert(allocatorCapacityCorrect),
            assert(buffer_capacity == CAPACITY),
            stutter,
        })
    }
    
    /// Test: INV-MEM-04 — allocator capacity is stable across operations
    run allocatorCapacityStableAcrossOps = {
        init
        .then(producerWrite)
        .then(producerWrite)
        .then(consumerAdvance)
        .then(all {
            assert(allocatorCapacityCorrect),
            assert(buffer_capacity == CAPACITY),
            stutter,
        })
    }
    
    /// Test: INV-ALLOC-01 — alignment guarantee holds at init
    run alignmentAtInit = {
        init.then(all {
            assert(alignmentGuarantee),
            assert(buffer_aligned == true),
            stutter,
        })
    }
    
    /// Test: INV-ALLOC-02 — zero overhead default at init
    run zeroOverheadAtInit = {
        init.then(all {
            assert(zeroOverheadDefault),
            assert(allocator_zst == true),
            stutter,
        })
    }
    
    /// Test: INV-INIT-01 — producer write initializes the correct slot
    run producerWriteInitializesSlot = {
        init
        .then(producerWrite)
        .then(all {
            assert(initializedRange),
            assert(initialized == Set(0)),
            stutter,
        })
        .then(producerWrite)
        .then(all {
            assert(initializedRange),
            assert(initialized == Set(0, 1)),
            stutter,
        })
    }
    
    /// Test: INV-INIT-01 — consumer advance uninitializes the correct slot
    run consumerAdvanceUninitializesSlot = {
        init
        .then(producerWrite)
        .then(producerWrite)
        .then(producerWrite)
        .then(all {
            assert(initialized == Set(0, 1, 2)),
            stutter,
        })
        .then(consumerAdvance)
        .then(all {
            assert(initializedRange),
            assert(initialized == Set(1, 2)),
            stutter,
        })
        .then(consumerAdvance)
        .then(all {
            assert(initializedRange),
            assert(initialized == Set(2)),
            stutter,
        })
    }
    
    /// Test: INV-INIT-01 — initialized range after wrap-around
    /// Exercises the modular arithmetic: after filling and partially
    /// draining, new writes wrap around to index 0.
    run initializedRangeWrapAround = {
        init
        // Fill all 4 slots (indices 0,1,2,3)
        .then(producerWrite)
        .then(producerWrite)
        .then(producerWrite)
        .then(producerWrite)
        .then(all {
            assert(initialized == Set(0, 1, 2, 3)),
            stutter,
        })
        // Consume 2 (free indices 0,1)
        .then(consumerAdvance)
        .then(consumerAdvance)
        .then(all {
            assert(initialized == Set(2, 3)),
            stutter,
        })
        // Write 2 more — these wrap around to indices 0,1 (tl=4→idx 0, tl=5→idx 1)
        .then(producerWrite)
        .then(producerWrite)
        .then(all {
            assert(initializedRange),
            assert(initialized == Set(0, 1, 2, 3)),
            assert(tl == 6),
            assert(hd == 2),
            stutter,
        })
    }
    
    /// Test: INV-INIT-01 — empty ring has no initialized slots
    run emptyRingNoInitializedSlots = {
        init
        .then(producerWrite)
        .then(consumerAdvance)
        .then(all {
            assert(initializedRange),
            assert(initialized == Set()),
            assert(tl == hd),
            stutter,
        })
    }
    
    /// Test: All allocator invariants hold through a full produce-consume cycle
    run allocatorInvariantsThroughCycle = {
        init
        .then(all { assert(safetyInvariant), stutter, })
        .then(producerWrite)
        .then(all { assert(safetyInvariant), stutter, })
        .then(producerWrite)
        .then(all { assert(safetyInvariant), stutter, })
        .then(consumerAdvance)
        .then(all { assert(safetyInvariant), stutter, })
        .then(producerWrite)
        .then(all { assert(safetyInvariant), stutter, })
        .then(consumerAdvance)
        .then(all { assert(safetyInvariant), stutter, })
        .then(consumerAdvance)
        .then(all { assert(safetyInvariant), stutter, })
    }

    // =====================================================================
    // NUMA INVARIANT TESTS (INV-NUMA-01, INV-NUMA-02, INV-NUMA-03)
    // =====================================================================

    /// Test: INV-NUMA-01 — NUMA placement valid at init
    run numaPlacementAtInit = {
        init.then(all {
            assert(numaPlacementValid),
            assert(numa_placement_valid == true),
            stutter,
        })
    }

    /// Test: INV-NUMA-02 — NUMA fallback safe at init
    run numaFallbackAtInit = {
        init.then(all {
            assert(numaFallbackSafe),
            assert(numa_fallback_safe == true),
            stutter,
        })
    }

    /// Test: INV-NUMA-03 — NUMA policy deterministic at init
    run numaPolicyAtInit = {
        init.then(all {
            assert(numaPolicyDeterministic),
            assert(numa_policy_deterministic == true),
            stutter,
        })
    }

    /// Test: NUMA invariants stable across produce-consume operations
    run numaInvariantsStableAcrossOps = {
        init
        .then(producerWrite)
        .then(producerWrite)
        .then(consumerAdvance)
        .then(all {
            assert(numaPlacementValid),
            assert(numaFallbackSafe),
            assert(numaPolicyDeterministic),
            stutter,
        })
    }
}
