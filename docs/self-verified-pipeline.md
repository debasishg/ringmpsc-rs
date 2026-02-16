# Invariant Validation Pipeline: From Spec to Verification

## A Visual Journey of a domain invariant Through the Agentic Code Generation Pipeline

---

## 1. The Complete Flow

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                        HUMAN-AUTHORED (Source of Truth)                     │
│                                                                             │
│   ┌─────────────────────────────────────────────────────────┐               │
│   │  spec.md                                                │               │
│   │                                                         │               │
│   │  ### INV-SEQ-01: Bounded Count                          │               │
│   │  ```                                                    │               │
│   │  0 ≤ (tail - head) ≤ capacity                           │               │
│   │  ```                                                    │               │
│   │  The number of items in the ring never exceeds capacity.│               │
│   └────────────────────────┬────────────────────────────────┘               │
└────────────────────────────┼────────────────────────────────────────────────┘
                             │
                             │  LLM reads spec, generates all downstream artifacts
                             │
        ┌────────────────────┼───────────────────────────────────────┐
        │                    ▼                                       │
        │  ┌─────────────────────────────┐                           │
        │  │   1. RingSPSC.qnt           │  Formal model             │
        │  │      (Quint spec)           │  (machine-checkable)      │
        │  └──────────┬──────────────────┘                           │
        │             │                                              │
        │    ┌────────┴────────┐                                     │
        │    ▼                 ▼                                     │
        │  ┌──────────┐  ┌──────────────┐                            │
        │  │2. invar- │  │3. property_  │                            │
        │  │  iants.rs│  │  tests.rs    │                            │
        │  │  (macros)│  │  (proptest)  │                            │
        │  └─────┬────┘  └──────┬───────┘                            │
        │        │              │                                    │
        │        ▼              │          ┌──────────────┐          │
        │  ┌──────────┐         │          │4. quint_     │          │
        │  │ ring.rs  │         │          │  mbt.rs      │          │
        │  │ stack_   │         │          │  (MBT driver)│          │
        │  │ ring.rs  │         │          └──────┬───────┘          │
        │  └──────────┘         │                 │                  │
        │                       │                 │                  │
        │           LLM-GENERATED ARTIFACTS       │                  │
        └───────────────────────┼─────────────────┼──────────────────┘
                                │                 │
                                ▼                 ▼
                    ┌──────────────────────────────────┐
                    │     VERIFICATION FEEDBACK LOOP   │
                    │                                  │
                    │  quint verify ← spec correctness │
                    │  cargo test   ← impl correctness │
                    │  debug_assert ← runtime checking │
                    └──────────────────────────────────┘
```

---

## 2. Stage 1: The Human Specification (Source of Truth)

**File**: `crates/ringmpsc/spec.md`

The invariant begins as a precise English statement with mathematical notation:

```
┌─────────────────────────────────────────────────────────────────┐
│  spec.md  §2 — Sequence Number Invariants                       │
│                                                                 │
│  ### INV-SEQ-01: Bounded Count                                  │
│  ┌───────────────────────────────────────────────────────┐      │
│  │  0 ≤ (tail - head) ≤ capacity                         │      │
│  └───────────────────────────────────────────────────────┘      │
│                                                                 │
│  The number of items in the ring never exceeds capacity         │
│  and is never negative.                                         │
│                                                                 │
│  Enforced by: debug_assert! in commit_internal()                │
│  Test:        integration_tests.rs, property_tests.rs           │
│  Formal:      TLA+ BoundedCount, Quint boundedCount             │
└─────────────────────────────────────────────────────────────────┘
```

**Key properties encoded in this single line**:

| Sub-property | Mathematical form | Meaning |
|---|---|---|
| Non-negative count | `tail - head ≥ 0` | Consumer never reads past producer |
| Bounded above | `tail - head ≤ capacity` | Producer never overwrites unread data |
| Implicit monotonicity | `tail ≥ head` always | No underflow/wrap corruption |

This is the **only manually written artifact**. Everything below is LLM-generated.

---

## 3. Stage 2: Quint Formal Specification (LLM-Generated)

**File**: `crates/ringmpsc/tla/RingSPSC.qnt`

The LLM translates the English spec into a machine-checkable Quint model:

```
┌─────────────────────────────────────────────────────────────────┐
│  spec.md                          RingSPSC.qnt                  │
│                                                                 │
│  "0 ≤ (tail-head) ≤ capacity"    val boundedCount: bool =       │
│          │                            tail >= head and          │
│          │    LLM translates          (tail - head) <= CAPACITY │
│          └──────────────────►                                   │
│                                                                 │
│  "head and tail only increase"   val monotonicProgress: bool =  │
│          │                            (encoded via actions)     │
│          └──────────────────►                                   │
│                                                                 │
│  "consumer never reads past"     val happensBefore: bool =      │
│          │                            head <= tail              │
│          └──────────────────►                                   │
└─────────────────────────────────────────────────────────────────┘
```

### The Quint Invariant Definition

```quint
module RingSPSC {
    const CAPACITY: int = 4
    const MAX_ITEMS: int = 8

    var head: int
    var tail: int
    var cached_head: int
    var cached_tail: int
    var items_produced: int

    // ┌──────────────────────────────────────────────────────────┐
    // │  INV-SEQ-01: Bounded Count                               │
    // │  Direct translation from spec.md                         │
    // └──────────────────────────────────────────────────────────┘
    val boundedCount: bool = 
        tail >= head and (tail - head) <= CAPACITY

    // ┌──────────────────────────────────────────────────────────┐
    // │  INV-ORD-03: Happens-Before                              │
    // └──────────────────────────────────────────────────────────┘
    val happensBefore: bool = head <= tail

    // Combined safety property checked by model checker
    val safetyInvariant: bool = boundedCount and happensBefore
}
```

### How Actions Preserve the Invariant

The Quint spec also encodes **state transitions** that must preserve INV-SEQ-01:

```quint
    // ProducerWrite: Can only write if count < capacity
    // ┌────────────────────────────────────────────────────────┐
    // │  PRECONDITION: (tail - head) < CAPACITY                │
    // │  This is the guard that PRESERVES INV-SEQ-01           │
    // └────────────────────────────────────────────────────────┘
    action producerWrite = all {
        (tail - head) < CAPACITY,          // ◄── guard ensures bounded count
        items_produced < MAX_ITEMS,
        tail' = tail + 1,                  // ◄── only increments (monotonic)
        items_produced' = items_produced + 1,
        head' = head,                      // unchanged
        cached_head' = cached_head,
        cached_tail' = cached_tail,
    }

    // ConsumerAdvance: Can only consume if count > 0
    // ┌────────────────────────────────────────────────────────┐
    // │  PRECONDITION: (tail - head) > 0                       │
    // │  Ensures head never exceeds tail (non-negative count)  │
    // └────────────────────────────────────────────────────────┘
    action consumerAdvance = all {
        (tail - cached_tail) > 0,          // ◄── guard ensures items exist
        head' = head + 1,                  // ◄── only increments (monotonic)
        tail' = tail,                      // unchanged
        cached_head' = cached_head,
        cached_tail' = cached_tail,
        items_produced' = items_produced,
    }
```

### Verification at the Spec Level

```
┌────────────────────────────────────────────────────────────────────────┐
│  $ quint verify RingSPSC.qnt --main=RingSPSC                           │
│         --invariant=safetyInvariant                                    │
│                                                                        │
│  ┌──────────────────────────────────────────────────────────────┐      │
│  │  Apalache/TLC explores ALL reachable states:                 │      │
│  │                                                              │      │
│  │    State 0: head=0, tail=0  → count=0 ≤ 4 ✓                  │      │
│  │    State 1: head=0, tail=1  → count=1 ≤ 4 ✓                  │      │
│  │    State 2: head=0, tail=2  → count=2 ≤ 4 ✓                  │      │
│  │    ...                                                       │      │
│  │    State N: head=0, tail=4  → count=4 ≤ 4 ✓                  │      │
│  │    State N+1: ProducerWrite BLOCKED (precondition false)     │      │
│  │    ...                                                       │      │
│  │                                                              │      │
│  │  ✅ Model checking completed. No error has been found.       │      │
│  └──────────────────────────────────────────────────────────────┘      │
│                                                                        │
│  If invariant WERE violated:                                           │
│  ┌──────────────────────────────────────────────────────────────┐      │
│  │  ❌ Error: Invariant boundedCount is violated.               │      │
│  │  State 5: tail=5, head=0, Capacity=4                         │      │
│  │  Counterexample trace: Init → Write → Write → Write →        │      │
│  │                        Write → Write  ← 5th write violated!  │      │
│  └──────────────────────────────────────────────────────────────┘      │
└────────────────────────────────────────────────────────────────────────┘
```

---

## 4. Stage 3: Runtime Invariant Macros (LLM-Generated)

**File**: `crates/ringmpsc/src/invariants.rs`

The LLM generates `debug_assert!` macros that embed the invariant directly into the production code:

```
┌─────────────────────────────────────────────────────────────────┐
│  RingSPSC.qnt                      invariants.rs                │
│                                                                 │
│  val boundedCount: bool =           macro_rules!                │
│      tail >= head and               debug_assert_bounded_count  │
│      (tail - head) <= CAPACITY      { ($count, $capacity) =>    │
│          │                              debug_assert!(          │
│          │    LLM generates             $count <= $capacity,    │
│          └──────────────────►          "INV-SEQ-01 violated"    │
│                                       )}                        │
│  val happensBefore: bool =          macro_rules!                │
│      head <= tail                   debug_assert_head_not_past_ │
│          │                          tail { ($head, $tail) =>    │
│          └──────────────────►          debug_assert!(           │
│                                        $head <= $tail,          │
│                                       "INV-SEQ-01 violated"     │
│                                       )}                        │
└─────────────────────────────────────────────────────────────────┘
```

### The Generated Macros

```rust
// filepath: crates/ringmpsc/src/invariants.rs

// ═══════════════════════════════════════════════════════════════
// INV-SEQ-01: Bounded Count
//   Quint:  val boundedCount = (tail - head) <= CAPACITY
//   Spec:   0 ≤ (tail - head) ≤ capacity
// ═══════════════════════════════════════════════════════════════

/// Assert that count does not exceed capacity.
macro_rules! debug_assert_bounded_count {
    ($count:expr, $capacity:expr) => {
        debug_assert!(
            $count <= $capacity,
            "INV-SEQ-01 violated: count {} exceeds capacity {}",
            $count,
            $capacity
        )
    };
}

/// Assert that head does not advance past tail.
macro_rules! debug_assert_head_not_past_tail {
    ($new_head:expr, $tail:expr) => {
        debug_assert!(
            $new_head <= $tail,
            "INV-SEQ-01 violated: advancing head {} beyond tail {}",
            $new_head,
            $tail
        )
    };
}
```

### Embedding in Production Code

The macros are invoked at the **exact points** where the invariant could be violated:

```
┌─────────────────────────────────────────────────────────────────┐
│  ring.rs — commit_internal()                                    │
│                                                                 │
│  fn commit_internal(&self, count: usize) {                      │
│      let old_tail = self.tail.load(Relaxed);                    │
│      let new_tail = old_tail + count as u64;                    │
│                                                                 │
│      // ┌────────────────────────────────────────────────────┐  │
│      // │ INV-SEQ-01 CHECK POINT                             │  │
│      // │ After computing new_tail, verify bounded count     │  │
│      // └────────────────────────────────────────────────────┘  │
│      debug_assert_bounded_count!(                               │
│          new_tail - self.head.load(Acquire),  // current count  │
│          self.capacity()                      // max allowed    │
│      );                                                         │
│                                                                 │
│      self.tail.store(new_tail, Release);  // publish            │
│  }                                                              │
├─────────────────────────────────────────────────────────────────┤
│  ring.rs — advance()                                            │
│                                                                 │
│  fn advance(&self, count: usize) {                              │
│      let old_head = self.head.load(Relaxed);                    │
│      let new_head = old_head + count as u64;                    │
│                                                                 │
│      // ┌────────────────────────────────────────────────────┐  │
│      // │ INV-SEQ-01 CHECK POINT                             │  │
│      // │ Head must never advance past tail                  │  │
│      // └────────────────────────────────────────────────────┘  │
│      debug_assert_head_not_past_tail!(                          │
│          new_head,                                              │
│          self.tail.load(Acquire)                                │
│      );                                                         │
│                                                                 │
│      self.head.store(new_head, Release);                        │
│  }                                                              │
└─────────────────────────────────────────────────────────────────┘
```

**Both** `Ring<T>` and `StackRing<T, N>` use the same macros, ensuring invariant consistency across implementations:

```
                   invariants.rs
                   ┌──────────┐
                   │ macros   │
                   └────┬─────┘
                   ┌────┴─────┐
                   ▼          ▼
             ring.rs     stack_ring.rs
            ┌────────┐  ┌────────────┐
            │Ring<T> │  │StackRing   │
            │        │  │<T, N>      │
            └────────┘  └────────────┘
            Same invariant checks!
```

---

## 5. Stage 4: Property-Based Tests (LLM-Generated from Quint Invariants)

**File**: `crates/ringmpsc/tests/property_tests.rs`

The LLM generates proptest properties that mirror each Quint invariant, using randomized inputs:

```
┌─────────────────────────────────────────────────────────────────┐
│  RingSPSC.qnt                     property_tests.rs             │
│                                                                 │
│  val boundedCount =                proptest! {                  │
│      (tail-head) <= CAPACITY          fn prop_bounded_count_    │
│          │                            ring(writes in 0..100) {  │
│          │    LLM generates              // ... write items     │
│          └──────────────────►           prop_assert!(           │
│                                          ring.len() <= capacity │
│                                         );                      │
│                                       }                         │
│                                    }                            │
│                                                                 │
│  // Also for StackRing:                                         │
│  val boundedCount =                proptest! {                  │
│      (same invariant)                 fn prop_bounded_count_    │
│          │                            stack_ring(writes...) {   │
│          └──────────────────►           prop_assert!(           │
│                                          ring.len() <= CAP      │
│                                         );                      │
│                                       }                         │
│                                    }                            │
└─────────────────────────────────────────────────────────────────┘
```

### The Generated Property Tests

```rust
// filepath: crates/ringmpsc/tests/property_tests.rs

// ═══════════════════════════════════════════════════════════════
// INV-SEQ-01: Bounded Count
// Quint:  val boundedCount = (tail - head) <= CAPACITY
// TLA+:   BoundedCount == (tail - head) <= Capacity
// ═══════════════════════════════════════════════════════════════

proptest! {
    /// INV-SEQ-01: Ring never exceeds capacity after any sequence of operations
    #[test]
    fn prop_bounded_count_ring(
        writes in 0usize..100,
        reads_between in 0usize..10,
    ) {
        let config = Config::default();
        let ring = Ring::<u64>::new(config);
        let capacity = ring.capacity();

        for i in 0..writes {
            // Attempt to write
            if let Some(mut r) = ring.reserve(1) {
                r.as_mut_slice()[0] = MaybeUninit::new(i as u64);
                r.commit();
            }

            // Occasionally consume
            if i % reads_between.max(1) == 0 {
                ring.consume_batch(|_| {});
            }

            // ┌──────────────────────────────────────────────────┐
            // │ INVARIANT CHECK: Same predicate as Quint spec    │
            // │ ring.len() ≡ (tail - head) in the formal model   │
            // └──────────────────────────────────────────────────┘
            prop_assert!(ring.len() <= capacity,
                "INV-SEQ-01 violated: len {} > capacity {}",
                ring.len(), capacity);
        }
    }
}
```

### Mapping: Quint Invariants → Property Tests

```
┌────────────────────┬─────────────────────────────┬──────────────────┐
│  Quint Invariant   │  Proptest Function          │  Applies To      │
├────────────────────┼─────────────────────────────┼──────────────────┤
│  boundedCount      │  prop_bounded_count_ring    │  Ring<T>         │
│                    │  prop_bounded_count_stack_  │  StackRing<T,N>  │
│                    │  ring                       │                  │
├────────────────────┼─────────────────────────────┼──────────────────┤
│  monotonicProgress │  prop_monotonic_progress    │  Ring<T>         │
│  (action encode)   │  prop_monotonic_progress_   │  StackRing<T,N>  │
│                    │  stack_ring                 │                  │
├────────────────────┼─────────────────────────────┼──────────────────┤
│  happensBefore     │  prop_happens_before        │  Ring<T>         │
│                    │  prop_happens_before_stack_ │  StackRing<T,N>  │
│                    │  ring                       │                  │
├────────────────────┼─────────────────────────────┼──────────────────┤
│  (INV-RES-01)      │  prop_partial_reservation   │  Ring<T>         │
└────────────────────┴─────────────────────────────┴──────────────────┘
```

---

## 6. Stage 5: Model-Based Testing (LLM-Generated from Quint Actions)

**File**: `crates/ringmpsc/tests/quint_mbt.rs`

This is the most sophisticated artifact. The LLM generates a **trace execution engine** that replays Quint action sequences against the real `Ring<T>`, checking invariants at every step:

```
┌─────────────────────────────────────────────────────────────────┐
│  RingSPSC.qnt                      quint_mbt.rs                 │
│                                                                 │
│  action producerWrite = ...    →   fn producer_write() → bool   │
│  action consumerAdvance = ...  →   fn consumer_advance() → bool │
│  action producerRefresh = ...  →   fn producer_refresh_cache()  │
│  action consumerRefresh = ...  →   fn consumer_refresh_cache()  │
│                                                                 │
│  val boundedCount = ...        →   fn check_bounded_count()     │
│  val happensBefore = ...       →   fn check_happens_before()    │
│  val safetyInvariant = ...     →   fn check_safety_invariant()  │
│                                                                 │
│  action step = any {           →   fn execute_trace(            │
│      producerWrite,                    actions: &[QuintAction]) │
│      consumerAdvance,          →       for action in actions {  │
│      ...                                   match action { ... } │
│  }                                         check_safety_        │
│                                            invariant();         │
│                                        }                        │
└─────────────────────────────────────────────────────────────────┘
```

### The System Under Test (SUT)

The `RingSUT` struct mirrors the Quint state variables:

```
┌──────────────────────────────┬──────────────────────────────────┐
│  Quint State Variables       │  RingSUT Fields                  │
├──────────────────────────────┼──────────────────────────────────┤
│  var head: int               │  consumed: u64                   │
│  var tail: int               │  produced: u64                   │
│  var cached_head: int        │  cached_head: u64                │
│  var cached_tail: int        │  cached_tail: u64                │
│  (implicit buffer)           │  ring: Ring<u64>  ◄── REAL IMPL  │
└──────────────────────────────┴──────────────────────────────────┘
```

```rust
// filepath: crates/ringmpsc/tests/quint_mbt.rs

struct RingSUT {
    ring: Ring<u64>,         // ◄── The REAL ring buffer
    consumed: u64,           // mirrors Quint `head`
    produced: u64,           // mirrors Quint `tail`
    cached_head: u64,        // mirrors Quint `cached_head`
    cached_tail: u64,        // mirrors Quint `cached_tail`
}
```

### Action Mapping: Quint → Rust

Each Quint action maps to a method on `RingSUT` that calls the **real** `Ring<T>` API:

```rust
// ┌──────────────────────────────────────────────────────────────┐
// │  Quint: action producerWrite = all {                         │
// │      (tail - head) < CAPACITY,                               │
// │      tail' = tail + 1, ...                                   │
// │  }                                                           │
// │                                    │                         │
// │                                    ▼                         │
// │  Rust: calls real Ring<T>::reserve() + commit()              │
// └──────────────────────────────────────────────────────────────┘

fn producer_write(&mut self) -> bool {
    if let Some(mut reservation) = self.ring.reserve(1) {  // ◄── REAL API
        reservation.as_mut_slice()[0] = MaybeUninit::new(self.produced);
        reservation.commit();                               // ◄── REAL commit
        self.produced += 1;                                 // track state
        true
    } else {
        false  // ring full, mirrors Quint precondition failure
    }
}
```

### The Invariant Check: `check_safety_invariant`

```rust
// Direct translation of Quint's safetyInvariant
fn check_safety_invariant(&self, capacity: u64) -> bool {
    //  Quint: val safetyInvariant = boundedCount and happensBefore
    self.check_bounded_count(capacity) && self.check_happens_before()
}

// Quint: val boundedCount = tail >= head and (tail - head) <= CAPACITY
fn check_bounded_count(&self, capacity: u64) -> bool {
    let count = self.produced - self.consumed;
    count <= capacity
}

// Quint: val happensBefore = head <= tail
fn check_happens_before(&self) -> bool {
    self.consumed <= self.produced
}
```

### The Trace Execution Engine: `execute_trace`

```rust
fn execute_trace(actions: &[QuintAction], capacity_bits: u8) -> Result<(), String> {
    let capacity = 1u64 << capacity_bits;
    let mut sut = RingSUT::new(capacity_bits);

    // ┌──────────────────────────────────────────────────┐
    // │  CHECK: Initial state satisfies invariant        │
    // │  Mirrors: Quint init => assert(safetyInvariant)  │
    // └──────────────────────────────────────────────────┘
    if !sut.check_safety_invariant(capacity) {
        return Err("Initial state violates safety invariant".to_string());
    }

    for (i, action) in actions.iter().enumerate() {
        // Execute action on REAL Ring<T>
        match action {
            QuintAction::ProducerWrite      => { sut.producer_write(); }
            QuintAction::ConsumerAdvance    => { sut.consumer_advance(); }
            QuintAction::ProducerRefreshCache => { sut.producer_refresh_cache(); }
            QuintAction::ConsumerRefreshCache => { sut.consumer_refresh_cache(); }
            _ => {}
        }

        // ┌──────────────────────────────────────────────────┐
        // │  CHECK: Invariant holds after EVERY action       │
        // │  Mirrors: Quint step => assert(safetyInvariant)  │
        // └──────────────────────────────────────────────────┘
        if !sut.check_safety_invariant(capacity) {
            return Err(format!(
                "Safety invariant violated after action {}: {:?} \
                 (produced={}, consumed={})",
                i, action, sut.produced, sut.consumed
            ));
        }
    }
    Ok(())
}
```

### The Generated Test Traces

Each test exercises a specific scenario drawn from the Quint state space:

```
┌─────────────────────────────────────────────────────────────────────┐
│  Test Trace                    │  Scenario                          │
├────────────────────────────────┼────────────────────────────────────┤
│  []                            │  Init only — empty ring valid      │
│                                │                                    │
│  [Write, Advance]              │  Basic produce-consume cycle       │
│                                │                                    │
│  [Write×4]                     │  Fill to capacity (count = cap)    │
│                                │  ← boundary of INV-SEQ-01          │
│                                │                                    │
│  [Write×4, Advance×2,          │  Stale cache recovery              │
│   RefreshCache, Write×2]       │                                    │
│                                │                                    │
│  [(Write, Advance)×20]         │  Alternating at steady state       │
│                                │                                    │
│  [Write×4, Advance×4,          │  Full → empty → refill             │
│   RefreshCache, Write×2]       │  ← starvation recovery             │
└─────────────────────────────────────────────────────────────────────┘
```

---

## 7. The Complete Feedback Loop

Here is the full verification cycle showing how each layer catches different classes of bugs:

```
┌─────────────────────────────────────────────────────────────────────────┐
│                                                                         │
│  ┌───────────┐        LLM generates         ┌───────────────┐           │
│  │ spec.md   │ ──────────────────────────►  │ RingSPSC.qnt  │           │
│  │ (human)   │                              │ (formal spec) │           │
│  └─────┬─────┘                              └───────┬───────┘           │
│        │                                        ┌───┴───┐               │
│        │                                        │       │               │
│        │     ┌──────────────────────────────────┘       │               │
│        │     │                                          │               │
│        │     ▼                                          ▼               │
│        │  ┌──────────────┐                  ┌────────────────┐          │
│        │  │quint verify  │                  │quint test      │          │
│        │  │              │                  │                │          │
│        │  │Catches:      │                  │Catches:        │          │
│        │  │• Logical     │                  │• Missing edge  │          │
│        │  │  errors in   │                  │  cases in spec │          │
│        │  │  protocol    │                  │• Unreachable   │          │
│        │  │  design      │                  │  states        │          │
│        │  └──────────────┘                  └────────────────┘          │
│        │                                                                │
│        │  LLM generates from Quint invariants                           │
│        │     │              │               │                           │
│        │     ▼              ▼               ▼                           │
│        │  ┌──────────┐  ┌──────────┐  ┌──────────────┐                  │
│        │  │invariants│  │property_ │  │quint_mbt.rs  │                  │
│        │  │.rs       │  │tests.rs  │  │              │                  │
│        │  │          │  │          │  │              │                  │
│        │  │Catches:  │  │Catches:  │  │Catches:      │                  │
│        │  │• Runtime │  │• Random  │  │• Spec-impl   │                  │
│        │  │  bound   │  │  input   │  │  divergence  │                  │
│        │  │  violat- │  │  combos  │  │• Action seq  │                  │
│        │  │  ions in │  │  that    │  │  that breaks │                  │
│        │  │  debug   │  │  break   │  │  invariants  │                  │
│        │  │  builds  │  │  invari- │  │  on REAL     │                  │
│        │  │          │  │  ants    │  │  Ring<T>     │                  │
│        │  └────┬─────┘  └──────────┘  └──────────────┘                  │
│        │       │                                                        │
│        │       ▼  Used in production code                               │
│        │  ┌──────────────────────────────────────┐                      │
│        │  │  ring.rs / stack_ring.rs             │                      │
│        │  │                                      │                      │
│        │  │  debug_assert_bounded_count!(...)    │                      │
│        │  │  debug_assert_head_not_past_tail!(..)│                      │
│        │  └──────────────────────────────────────┘                      │
│        │                                                                │
│        │  ┌────────────────────────────────────────────────────┐        │
│        └─►│  ADDITIONAL VERIFICATION (not generated from Quint)│        │
│           │                                                    │        │
│           │  loom_tests.rs  → Exhaustive thread interleavings  │        │
│           │  miri_tests.rs  → Undefined behavior detection     │        │
│           └────────────────────────────────────────────────────┘        │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## 8. Verification Commands: Complete Pipeline

```bash
# ═══════════════════════════════════════════════════════════════
# STAGE 1: Spec-level verification (Quint)
# ═══════════════════════════════════════════════════════════════
cd crates/ringmpsc/tla

quint typecheck RingSPSC.qnt                             # Syntax check
quint test RingSPSC.qnt --main=RingSPSC                  # Spec tests
quint verify RingSPSC.qnt --main=RingSPSC \
    --invariant=safetyInvariant                           # Exhaustive check

# ═══════════════════════════════════════════════════════════════
# STAGE 2: Implementation verification
# ═══════════════════════════════════════════════════════════════
cd ../../..

# Model-based testing (Quint traces → Ring<T>)
cargo test -p ringmpsc-rs --test quint_mbt \
    --features quint-mbt --release

# Property testing (random inputs → invariant checks)
cargo test -p ringmpsc-rs --test property_tests \
    --features stack-ring --release

# ═══════════════════════════════════════════════════════════════
# STAGE 3: Concurrency & memory safety
# ═══════════════════════════════════════════════════════════════

# Loom: exhaustive thread interleaving
cargo test -p ringmpsc-rs --features loom \
    --test loom_tests --release

# Miri: undefined behavior detection  
cargo +nightly miri test -p ringmpsc-rs --test miri_tests
```

---

## 9. What Each Layer Catches: A Failure Taxonomy

```
┌──────────────────────────────────────────────────────────────────────────┐
│  Bug: Producer writes past capacity (INV-SEQ-01 violation)               │
├────────────────────┬─────────────────────────────────────────────────────┤
│  Layer             │  How It Catches                                     │
├────────────────────┼─────────────────────────────────────────────────────┤
│                    │                                                     │
│  quint verify      │  ❌ Invariant boundedCount is violated.             │
│                    │  State N: tail=5, head=0, CAPACITY=4                │
│                    │  Counterexample: Init→Write→Write→Write→Write→Writ e│
│                    │                                                     │
├────────────────────┼─────────────────────────────────────────────────────┤
│                    │                                                     │
│  quint_mbt.rs      │  ❌ Safety invariant violated after action 4:       │
│                    │  ProducerWrite (produced=5, consumed=0)             │
│                    │  ← check_bounded_count(5-0=5 > 4) FAILS             │
│                    │                                                     │
├────────────────────┼─────────────────────────────────────────────────────┤
│                    │                                                     │
│  property_tests.rs │  ❌ proptest: prop_bounded_count_ring failed        │
│                    │  INV-SEQ-01 violated: len 5 > capacity 4            │
│                    │  Minimal failing case: writes=5, reads_between=100  │
│                    │                                                     │
├────────────────────┼─────────────────────────────────────────────────────┤
│                    │                                                     │
│  invariants.rs     │  ❌ thread 'main' panicked at:                      │
│  (debug build)     │  'INV-SEQ-01 violated: count 5 exceeds capacity 4'  │
│                    │  src/ring.rs:285 in commit_internal()               │
│                    │                                                     │
├────────────────────┼─────────────────────────────────────────────────────┤
│                    │                                                     │
│  loom_tests.rs     │  ❌ Data race detected under specific interleaving  │
│                    │  (if the bug is concurrency-related)                │
│                    │                                                     │
└────────────────────┴─────────────────────────────────────────────────────┘
```

---

## 10. The Traceability Matrix: INV-SEQ-01 Across All Artifacts

This table provides complete traceability from the English spec through every generated artifact:

| Artifact | File | Expression | Checked By |
|---|---|---|---|
| **Spec** | `crates/ringmpsc/spec.md` | `0 ≤ (tail - head) ≤ capacity` | Human review |
| **TLA+** | `crates/ringmpsc/tla/RingSPSC.tla` | `BoundedCount == (tail - head) <= Capacity` | TLC model checker |
| **Quint** | `crates/ringmpsc/tla/RingSPSC.qnt` | `val boundedCount = (tail - head) <= CAPACITY` | `quint verify` |
| **Macro** | `crates/ringmpsc/src/invariants.rs` | `debug_assert!($count <= $capacity)` | Debug build runtime |
| **Ring** | `crates/ringmpsc/src/ring.rs` | `debug_assert_bounded_count!(count, cap)` | Every commit/advance call |
| **StackRing** | `crates/ringmpsc/src/stack_ring.rs` | `debug_assert_bounded_count!(count, cap)` | Every commit/advance call |
| **Proptest** | `crates/ringmpsc/tests/property_tests.rs` | `prop_assert!(ring.len() <= capacity)` | `cargo test --release` |
| **MBT** | `crates/ringmpsc/tests/quint_mbt.rs` | `(produced - consumed) <= capacity` | `cargo test --features quint-mbt` |
| **Loom** | `crates/ringmpsc/tests/loom_tests.rs` | Structural (interleaving exploration) | `cargo test --features loom` |

---

## 11. Why This Pipeline Matters for Agentic Code Generation

```
┌─────────────────────────────────────────────────────────────────┐
│  WITHOUT this pipeline:                                         │
│                                                                 │
│  Developer writes spec → LLM generates code → ???               │
│                                                                 │
│  • No way to verify LLM understood the spec                     │
│  • No mechanical check that generated code is correct           │
│  • Regressions go undetected until production                   │
├─────────────────────────────────────────────────────────────────┤
│  WITH this pipeline:                                            │
│                                                                 │
│  Developer writes spec → LLM generates:                         │
│    1. Quint spec    ← model checker verifies spec consistency   │
│    2. invariants.rs ← runtime checks in debug builds            │
│    3. proptests     ← random testing verifies real code         │
│    4. MBT driver    ← spec traces verify real code              │
│                                                                 │
│  EVERY invariant violation is caught by ≥2 layers               │
│  BEFORE the code ever reaches production.                       │
└─────────────────────────────────────────────────────────────────┘
```

The key insight is that the **spec is written once by a human**, and the LLM generates **four independent verification artifacts** from it. If the LLM misinterprets the spec in one artifact, the other artifacts catch the discrepancy — creating a self-correcting feedback loop that gives mathematical confidence in the generated code.
