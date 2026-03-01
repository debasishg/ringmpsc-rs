# Formal Verification Workflow: TLA+ → Quint → Rust

This document explains the complete workflow for formal verification of the ringmpsc lock-free ring buffer, from TLA+ specifications through Quint model-based testing to actual Rust implementation verification.

> **Naming note**: The formal specification files are named `RingSPSC` because the spec models the
> single-producer single-consumer (SPSC) base protocol. The repository name `ringmpsc-rs` reflects
> the multi-producer extension built on top. See `QUINT_0_31_UPGRADE.md` for planned MPSC spec work.

## Overview

The verification strategy uses multiple complementary layers:

| Layer | Tool | Purpose |
|-------|------|---------|
| Formal Spec | TLA+ / Quint | Mathematical model of the protocol |
| Modern Spec | Quint (≥ 0.31.0) | Same semantics, Rust backend, TLC integration |
| Model Checking | `quint verify --backend=tlc` / Apalache | Exhaustive state space exploration |
| Model-Based Testing | quint_mbt.rs + `quint-connect` | Execute auto-generated spec traces on real code |
| Property Testing | proptest | Random input verification |
| Concurrency Testing | Loom | Exhaustive thread interleaving |
| Memory Safety | Miri | Undefined behavior detection |

## 1. TLA+ Specification (Source of Truth)

**File**: [tla/RingSPSC.tla](../crates/ringmpsc/tla/RingSPSC.tla)

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

**File**: [tla/RingSPSC.qnt](../crates/ringmpsc/tla/RingSPSC.qnt)

Quint is a modern specification language with TLA+ semantics but TypeScript-like syntax. It provides better tooling and IDE support.

### Translation Mapping

| TLA+ | Quint | Notes |
|------|-------|-------|
| `VARIABLE x` | `var x: int` | Quint requires types |
| `x' = expr` | `x' = expr` | Same primed notation |
| `\/ A \/ B` | `any { A, B }` | Nondeterministic choice |
| `/\ A /\ B` | `all { A, B }` | Conjunction |
| `Nat` | `int` | Quint uses bounded ints; note `int` includes negatives — add `x >= 0` guards if non-negativity is required |

> **Variable renaming**: Quint (≥ 0.30.0) reserves `head` and `tail` as built-in list operation
> names. The spec therefore uses `hd` (for `head`) and `tl` (for `tail`). All Quint examples
> in this document use the renamed variables.

### Quint Invariants

```quint
/// INV-SEQ-01: Bounded Count
val boundedCount: bool =
    tl >= hd and (tl - hd) <= CAPACITY

/// INV-ORD-03: Happens-Before
val happensBefore: bool = hd <= tl
```

### Quint Actions

```quint
/// ProducerWrite: Commit item to ring
action producerWrite = all {
    (tl - hd) < CAPACITY,
    items_produced < MAX_ITEMS,
    tl' = tl + 1,
    items_produced' = items_produced + 1,
    // Unchanged
    hd' = hd,
    cached_head' = cached_head,
    cached_tail' = cached_tail,
}
```

### Running Quint

```bash
# Install Quint (≥ 0.31.0)
npm install -g @informalsystems/quint

# Typecheck
quint typecheck RingSPSC.qnt

# Run embedded tests (Rust backend, default since 0.31.0)
quint test RingSPSC.qnt --main=RingSPSC

# Simulate random traces (Rust backend, default since 0.31.0)
quint run RingSPSC.qnt --main=RingSPSC --max-steps=100

# Simulation with invariant checking (Rust backend)
quint run RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant

# Exhaustive model checking via TLC backend (requires JDK 17+)
# Equivalent to running TLC on the .tla file, but directly from .qnt
quint verify RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant --backend=tlc

# Symbolic model checking via Apalache backend (requires JDK 17+)
quint verify RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant
```

## 3. Model-Based Testing (MBT)

**File**: [tests/quint_mbt.rs](../crates/ringmpsc/tests/quint_mbt.rs)

Model-based testing bridges the gap between specification and implementation. The current approach uses
[`quint-connect`](https://crates.io/crates/quint-connect) to **automatically generate** simulation
traces from the Quint spec and replay them against the real `Ring<T>`, with full state comparison at
every step.

> **Historical note**: An earlier approach used hand-crafted action sequences and manually
> re-implemented invariant checks in Rust. That approach has been superseded. See
> `model-based-testing-in-agentic-development.md` for the full evolution and rationale.

### Architecture

```
RingSPSC.qnt ──▶ quint run --mbt ──▶ ITF traces ──▶ quint-connect Driver ──▶ Ring<T>
 (spec)           (simulation)        (states +      (action dispatch +       (real impl)
                                      actions)       state comparison)
```

### State Struct (deserialized from ITF traces)

The `RingSPSCState` struct is automatically deserialized from Quint's ITF trace output.
After each step, `quint-connect` compares the driver's state with the expected trace state:

```rust
#[derive(Eq, PartialEq, Deserialize, Debug)]
struct RingSPSCState {
    #[serde(rename = "hd")]
    head: i64,          // Quint variable: hd
    #[serde(rename = "tl")]
    tail: i64,          // Quint variable: tl
    cached_head: i64,
    cached_tail: i64,
    items_produced: i64,
}

impl State<RingSPSCDriver> for RingSPSCState {
    fn from_driver(driver: &RingSPSCDriver) -> Result<Self> {
        Ok(RingSPSCState {
            head: driver.consumed as i64,
            tail: driver.produced as i64,
            cached_head: driver.cached_head as i64,
            cached_tail: driver.cached_tail as i64,
            items_produced: driver.items_produced as i64,
        })
    }
}
```

### Driver (connects Ring\<T\> to Quint actions)

```rust
impl Driver for RingSPSCDriver {
    type State = RingSPSCState;

    fn step(&mut self, step: &Step) -> Result {
        switch!(step {
            init => { *self = Self::default(); },

            producerWrite => {
                // Quint: tl' = tl + 1, items_produced' = items_produced + 1
                let mut reserved = self.ring.reserve(1)
                    .expect("Quint guard ensures capacity available");
                reserved.as_mut_slice()[0] = MaybeUninit::new(self.produced);
                reserved.commit();
                self.produced += 1;
                self.items_produced += 1;
            },

            consumerAdvance => {
                // Quint: hd' = hd + 1
                let consumed = self.ring.consume_up_to(1, |_| {});
                assert_eq!(consumed, 1, "Quint guard ensures items exist");
                self.consumed += 1;
            },

            // ... other actions ...
        })
    }
}
```

### Automated Trace Generation

Traces are **automatically generated** by the Quint simulator via `#[quint_run]`. Multiple seeds
provide diverse coverage:

```rust
// Default random simulation (seed controllable via QUINT_SEED env var)
#[test]
fn simulation() { /* uses runtime_seed() for reproducibility */ }

// Broader exploration with fixed seeds
#[quint_run(spec = "tla/RingSPSC.qnt", main = "RingSPSC", seed = "1729", max_samples = 20)]
fn simulation_deep() -> impl Driver {
    RingSPSCDriver::default()
}
```

### Running MBT Tests

```bash
cargo test -p ringmpsc-rs --test quint_mbt --features quint-mbt --release

# Verbose trace output
QUINT_VERBOSE=1 cargo test -p ringmpsc-rs --test quint_mbt --features quint-mbt --release -- --nocapture

# Reproduce a specific failing trace
QUINT_SEED=42 cargo test -p ringmpsc-rs --test quint_mbt --features quint-mbt --release
```

## 4. Property-Based Testing (Proptest)

**File**: [tests/property_tests.rs](../crates/ringmpsc/tests/property_tests.rs)

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
1. Write/Update Quint spec (RingSPSC.qnt) — single source of truth
   ↓
2. Run quint test / quint run --invariant=safetyInvariant (fast, Rust backend)
   ↓
3. Run quint verify --backend=tlc (exhaustive model checking)
   ↓
4. Update Driver::step dispatch table if new Quint actions were added (quint_mbt.rs)
   (Traces are auto-generated by quint-connect — no manual trace authoring needed)
   ↓
5. Update proptest properties (property_tests.rs)
   ↓
6. Run all tests
```

> **Note**: The TLA+ spec (`RingSPSC.tla`) is retained as a reference but the
> Quint spec is the primary artifact. Since Quint 0.31.0, `quint verify --backend=tlc`
> enables exhaustive model checking directly from `.qnt` files. The `.tla` file is
> additionally used for liveness properties (`~>` leads-to) not yet supported by Quint.

### CI/CD Commands

```bash
# Full verification suite
cargo test -p ringmpsc-rs --test property_tests --features stack-ring --release
cargo test -p ringmpsc-rs --test quint_mbt --features quint-mbt --release
cargo test -p ringmpsc-rs --features loom --test loom_tests --release
cargo +nightly miri test -p ringmpsc-rs --test miri_tests

# TLA+ model checking via Quint CLI (preferred, requires Quint ≥ 0.31.0 + JDK 17+)
cd crates/ringmpsc/tla
quint verify RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant --backend=tlc

# TLA+ model checking via standalone TLC (alternative, any JDK)
cd crates/ringmpsc/tla
tlc RingSPSC.tla -config RingSPSC.cfg -workers auto

# Quint simulation with invariant checking (Rust backend, fast)
quint run RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant

# Quint embedded tests
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

### At Implementation Level (quint-connect MBT)

If the Rust implementation diverges from the spec, `quint-connect` catches it immediately
with a state diff:

```
Expected state from Quint spec:
  RingSPSCState { head: 0, tail: 4, cached_head: 0, ... }

Actual state from Ring<T> driver:
  RingSPSCState { head: 0, tail: 5, cached_head: 0, ... }
                                    ^^^^^^ divergence detected after step: producerWrite
```

### At Runtime (debug_assert!)

The implementation includes `debug_assert!` macros that check invariants in debug builds:

```rust
// In ring.rs
debug_assert_bounded_count!(count, capacity);
debug_assert_head_not_past_tail!(head, tail);
```

## 7. Trust Hierarchy

```
┌─────────────────────────────────────────────────────────────────┐
│  HIGHEST TRUST: TLA+ / Quint Spec                               │
│  - Mathematical model, machine-checkable                        │
│  - quint verify --backend=tlc: exhaustive (955 states verified) │
│  - quint verify (Apalache): symbolic model checking via SMT     │
│  - .tla retained for EventuallyConsumed liveness (~>)           │
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

3. **quint-connect picks it up automatically**: No Rust invariant re-implementation
   needed — the spec IS the oracle. `State::from_driver()` comparison will catch any
   violation for the new invariant across all auto-generated traces.

4. **Add proptest** for targeted random coverage:
   ```rust
   proptest! {
       #[test]
       fn prop_new_invariant(...) { ... }
   }
   ```

### Adding New Actions

1. **TLA+**: Add action predicate to `Next`
2. **Quint**: Add action to `step`
3. **MBT**: Add a new arm to the `switch!` in `Driver::step()` — traces that include
   the new action will be generated automatically by `quint run --mbt`
4. **Tests**: Run the MBT suite; the new action will appear in auto-generated traces

## Summary

The formal verification workflow provides defense-in-depth:

| What | How | Catches |
|------|-----|---------|
| **Spec correctness** | TLC model checking (`quint verify --backend=tlc`) | Logical errors in protocol design |
| **Spec completeness** | Quint simulation (Rust backend) | Missing edge cases |
| **Implementation conformance** | `quint-connect` MBT | Divergence from spec (full state comparison) |
| **Random input handling** | Proptest | Unexpected input combinations |
| **Concurrency correctness** | Loom | Data races, ordering bugs |
| **Memory safety** | Miri | Undefined behavior |

This multi-layered approach provides high confidence that the ringmpsc implementation correctly implements the specified lock-free protocol.
