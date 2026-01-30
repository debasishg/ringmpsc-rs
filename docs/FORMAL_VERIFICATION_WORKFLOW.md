# Formal Verification Workflow: TLA+ → Quint → Rust

This document explains the complete workflow for formal verification of the ringmpsc lock-free ring buffer, from TLA+ specifications through Quint model-based testing to actual Rust implementation verification.

## Overview

The verification strategy uses multiple complementary layers:

| Layer | Tool | Purpose |
|-------|------|---------|
| Formal Spec | TLA+ | Mathematical model of the protocol |
| Modern Spec | Quint | Same semantics, better tooling |
| Model Checking | TLC / Apalache | Exhaustive state space exploration |
| Model-Based Testing | quint_mbt.rs | Execute spec traces on real code |
| Property Testing | proptest | Random input verification |
| Concurrency Testing | Loom | Exhaustive thread interleaving |
| Memory Safety | Miri | Undefined behavior detection |

## 1. TLA+ Specification (Source of Truth)

**File**: [tla/RingSPSC.tla](tla/RingSPSC.tla)

TLA+ is a formal specification language for concurrent and distributed systems. Our spec models the SPSC ring buffer protocol.

### State Variables

```tla
VARIABLES
    head,           \* Consumer's read position (only consumer writes)
    tail,           \* Producer's write position (only producer writes)
    cached_head,    \* Producer's cached view of head
    cached_tail,    \* Consumer's cached view of tail
    items_produced  \* Total items (for termination)
```

### Invariants (Safety Properties)

```tla
\* INV-SEQ-01: Bounded Count
BoundedCount == 
    /\ tail >= head
    /\ (tail - head) <= Capacity

\* INV-ORD-03: Happens-Before
HappensBefore == head <= tail
```

### Actions (State Transitions)

| Action | Description | Models |
|--------|-------------|--------|
| `ProducerReserveFast` | Check cached_head for space | `reserve()` fast path |
| `ProducerRefreshCache` | Reload head (Acquire) | `reserve()` slow path |
| `ProducerWrite` | Commit item (Release) | `commit_internal()` |
| `ConsumerRefreshCache` | Reload tail (Acquire) | `consume_batch()` slow path |
| `ConsumerAdvance` | Consume item (Release) | `advance()` |

### Running TLC Model Checker

```bash
cd crates/ringmpsc/tla
java -jar "/Applications/TLA+ Toolbox.app/Contents/Eclipse/tla2tools.jar" \
    RingSPSC.tla -config RingSPSC.cfg -workers auto
```

**Output**: Explores all reachable states, reports any invariant violations with counterexample traces.

## 2. Quint Translation

**File**: [tla/RingSPSC.qnt](tla/RingSPSC.qnt)

Quint is a modern specification language with TLA+ semantics but TypeScript-like syntax. It provides better tooling and IDE support.

### Translation Mapping

| TLA+ | Quint | Notes |
|------|-------|-------|
| `VARIABLE x` | `var x: int` | Quint requires types |
| `x' = expr` | `x' = expr` | Same primed notation |
| `\/ A \/ B` | `any { A, B }` | Nondeterministic choice |
| `/\ A /\ B` | `all { A, B }` | Conjunction |
| `Nat` | `int` | Quint uses bounded ints |

### Quint Invariants

```quint
/// INV-SEQ-01: Bounded Count
val boundedCount: bool = 
    tail >= head and (tail - head) <= CAPACITY

/// INV-ORD-03: Happens-Before
val happensBefore: bool = head <= tail
```

### Quint Actions

```quint
/// ProducerWrite: Commit item to ring
action producerWrite = all {
    (tail - head) < CAPACITY,
    items_produced < MAX_ITEMS,
    tail' = tail + 1,
    items_produced' = items_produced + 1,
    // Unchanged
    head' = head,
    cached_head' = cached_head,
    cached_tail' = cached_tail,
}
```

### Running Quint

```bash
# Install Quint
npm install -g @informalsystems/quint

# Typecheck
quint typecheck RingSPSC.qnt

# Run embedded tests
quint test RingSPSC.qnt --main=RingSPSC

# Simulate random traces
quint run RingSPSC.qnt --main=RingSPSC --max-steps=100

# Formal verification (via Apalache)
quint verify RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant
```

## 3. Model-Based Testing (MBT)

**File**: [tests/quint_mbt.rs](tests/quint_mbt.rs)

Model-based testing bridges the gap between specification and implementation. We execute action traces from the spec against the real `Ring<T>`.

### Architecture

```
┌─────────────────┐     ┌─────────────────┐     ┌─────────────────┐
│  RingSPSC.qnt   │────▶│  quint_mbt.rs   │────▶│    Ring<T>      │
│  (Quint spec)   │     │  (trace driver) │     │  (Rust impl)    │
└─────────────────┘     └─────────────────┘     └─────────────────┘
        │                       │                       │
        ▼                       ▼                       ▼
   Generate action        Execute each           Verify invariants
   sequences              action on Ring         after each step
```

### Action Mapping

The driver maps Quint actions to Rust method calls:

```rust
enum QuintAction {
    ProducerReserveFast,
    ProducerRefreshCache,
    ProducerWrite,
    ConsumerReadFast,
    ConsumerRefreshCache,
    ConsumerAdvance,
}

fn execute_action(sut: &mut RingSUT, action: QuintAction) {
    match action {
        ProducerWrite => {
            if let Some(mut r) = sut.ring.reserve(1) {
                r.as_mut_slice()[0] = MaybeUninit::new(sut.produced);
                r.commit();
                sut.produced += 1;
            }
        }
        ConsumerAdvance => {
            sut.ring.consume_batch(|_| {});
            sut.consumed += 1;
        }
        // ...
    }
}
```

### Invariant Verification

After each action, we verify the spec invariants hold:

```rust
fn check_safety_invariant(&self, capacity: u64) -> bool {
    // INV-SEQ-01: Bounded Count
    let bounded = (self.produced - self.consumed) <= capacity;
    
    // INV-ORD-03: Happens-Before
    let happens_before = self.consumed <= self.produced;
    
    bounded && happens_before
}
```

### Test Traces

We craft traces that exercise specific scenarios:

```rust
#[test]
fn test_cache_refresh_scenario() {
    let trace = vec![
        // Fill ring to capacity
        ProducerWrite, ProducerWrite, ProducerWrite, ProducerWrite,
        // Consumer drains
        ConsumerAdvance, ConsumerAdvance,
        // Producer's cache is stale, must refresh
        ProducerRefreshCache,
        // Now producer can write again
        ProducerWrite, ProducerWrite,
    ];
    execute_trace(&trace, 2).expect("Should pass");
}
```

### Running MBT Tests

```bash
cargo test -p ringmpsc-rs --test quint_mbt --features quint-mbt --release
```

## 4. Property-Based Testing (Proptest)

**File**: [tests/property_tests.rs](tests/property_tests.rs)

Property tests complement MBT by using random inputs to verify invariants hold under arbitrary conditions.

### TLA+ Invariant → Proptest

```rust
proptest! {
    /// INV-SEQ-01: Bounded Count
    #[test]
    fn prop_bounded_count_ring(writes in 0usize..100) {
        let ring = Ring::<u64>::new(Config::default());
        
        for i in 0..writes {
            if let Some(mut r) = ring.reserve(1) {
                r.as_mut_slice()[0] = MaybeUninit::new(i as u64);
                r.commit();
            }
        }
        
        // Invariant must hold
        prop_assert!(ring.len() <= capacity);
    }
}
```

### Coverage Mapping

| TLA+ Invariant | Proptest Function |
|----------------|-------------------|
| `BoundedCount` | `prop_bounded_count_ring`, `prop_bounded_count_stack_ring` |
| Monotonic progress | `prop_monotonic_progress`, `prop_monotonic_progress_stack_ring` |
| `HappensBefore` | `prop_happens_before`, `prop_happens_before_stack_ring` |
| INV-RES-01 | `prop_partial_reservation` |

### Running Property Tests

```bash
# Ring<T> only
cargo test -p ringmpsc-rs --test property_tests --release

# Ring<T> + StackRing<T, N>
cargo test -p ringmpsc-rs --test property_tests --features stack-ring --release
```

## 5. Complete Verification Pipeline

### Development Workflow

```
1. Write/Update TLA+ spec (RingSPSC.tla)
   ↓
2. Run TLC for exhaustive model checking
   ↓
3. Translate to Quint (RingSPSC.qnt)
   ↓
4. Run quint test / quint verify
   ↓
5. Update MBT driver traces (quint_mbt.rs)
   ↓
6. Update proptest properties (property_tests.rs)
   ↓
7. Run all tests
```

### CI/CD Commands

```bash
# Full verification suite
cargo test -p ringmpsc-rs --test property_tests --features stack-ring --release
cargo test -p ringmpsc-rs --test quint_mbt --features quint-mbt --release
cargo test -p ringmpsc-rs --features loom --test loom_tests --release
cargo +nightly miri test -p ringmpsc-rs --test miri_tests

# TLA+ model checking (manual or CI with TLA+ Toolbox)
cd crates/ringmpsc/tla
tlc RingSPSC.tla -config RingSPSC.cfg -workers auto

# Quint verification (if installed)
quint test RingSPSC.qnt --main=RingSPSC
```

## 6. How Invariant Violations Are Caught

### At Specification Level (TLC/Quint)

If an invariant can be violated by any sequence of actions, TLC produces a counterexample:

```
Error: Invariant BoundedCount is violated.
State 1: <Init>
  head = 0, tail = 0
State 2: <ProducerWrite>
  head = 0, tail = 1
State 3: <ProducerWrite>
  ...
State N: <ProducerWrite>
  head = 0, tail = 5  ← VIOLATION: 5 > Capacity(4)
```

### At Implementation Level (MBT/Proptest)

If the Rust implementation diverges from the spec:

```rust
// In quint_mbt.rs
fn execute_trace(actions: &[QuintAction], capacity_bits: u8) -> Result<(), String> {
    for (i, action) in actions.iter().enumerate() {
        execute_action(&mut sut, *action);
        
        if !sut.check_safety_invariant(capacity) {
            return Err(format!(
                "Safety invariant violated after action {}: {:?}",
                i, action
            ));
        }
    }
    Ok(())
}
```

### At Runtime (debug_assert!)

The implementation includes `debug_assert!` macros that check invariants in debug builds:

```rust
// In ring.rs
debug_assert_bounded_count!(count, capacity);
debug_assert_head_le_tail!(head, tail);
```

## 7. Trust Hierarchy

```
┌─────────────────────────────────────────────────────────────────┐
│  HIGHEST TRUST: TLA+ / Quint Spec                               │
│  - Mathematical model, machine-checkable                        │
│  - TLC explores ALL reachable states                            │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  HIGH TRUST: Loom Tests                                         │
│  - Exhaustive thread interleaving for small configs             │
│  - Catches data races, memory ordering bugs                     │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  MEDIUM TRUST: MBT + Proptest                                   │
│  - Tests spec conformance on real implementation                │
│  - Random/adversarial input generation                          │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  BASE TRUST: Integration + Unit Tests                           │
│  - Exercises happy paths and edge cases                         │
│  - Catches obvious bugs                                         │
└─────────────────────────────────────────────────────────────────┘
```

## 8. Extending the Workflow

### Adding New Invariants

1. **Add to TLA+ spec**:
   ```tla
   NewInvariant == some_condition
   SafetyInvariant == ... /\ NewInvariant
   ```

2. **Add to Quint spec**:
   ```quint
   val newInvariant: bool = some_condition
   val safetyInvariant: bool = ... and newInvariant
   ```

3. **Add to MBT driver**:
   ```rust
   fn check_new_invariant(&self) -> bool { ... }
   fn check_safety_invariant(&self) -> bool {
       ... && self.check_new_invariant()
   }
   ```

4. **Add proptest**:
   ```rust
   proptest! {
       #[test]
       fn prop_new_invariant(...) { ... }
   }
   ```

### Adding New Actions

1. **TLA+**: Add action predicate to `Next`
2. **Quint**: Add action to `step`
3. **MBT**: Add enum variant and execution logic
4. **Tests**: Add traces exercising the new action

## Summary

The formal verification workflow provides defense-in-depth:

| What | How | Catches |
|------|-----|---------|
| **Spec correctness** | TLC model checking | Logical errors in protocol design |
| **Spec completeness** | Quint simulation | Missing edge cases |
| **Implementation conformance** | MBT driver | Divergence from spec |
| **Random input handling** | Proptest | Unexpected input combinations |
| **Concurrency correctness** | Loom | Data races, ordering bugs |
| **Memory safety** | Miri | Undefined behavior |

This multi-layered approach provides high confidence that the ringmpsc implementation correctly implements the specified lock-free protocol.
