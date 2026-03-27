# Formal Methods for Agentic Development

> **Last updated**: 2026-03-01 | **Quint**: ≥ 0.31.0 | **quint-connect**: 0.1.1

This document explores how formal methods can be integrated into AI-assisted (agentic) software development to ensure correctness, maintain invariants, and bridge the gap between specifications and implementations.

## The Hierarchical Context: Rules → Spec → Code

Modern agentic development operates within a layered context hierarchy:

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              RULES LAYER                                    │
│                                                                             │
│  .claude/CLAUDE.md, AGENTS.md, workspace conventions                        │
│                                                                             │
│  "How to write code in this project"                                        │
│  - Memory ordering patterns                                                 │
│  - Error handling rules                                                     │
│  - Invariant module pattern                                                 │
│  - Testing requirements                                                     │
└─────────────────────────────────────────────────────────────────────────────┘
                                    │
                                    ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                              SPEC LAYER                                     │
│                                                                             │
│  spec.md - domain invariants with INV-* identifiers                         │
│                                                                             │
│  "What properties must always hold"                                         │
│  - INV-SEQ-01: Bounded Count                                                │
│  - INV-ORD-03: Happens-Before                                               │
│  - INV-SW-01: Single-Writer Ownership                                       │
└─────────────────────────────────────────────────────────────────────────────┘
                                    │
                                    ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                              CODE LAYER                                     │
│                                                                             │
│  Rust implementation with debug_assert! referencing INV-* IDs               │
│                                                                             │
│  "How properties are implemented and verified"                              │
│  - ring.rs: reserve(), commit_internal(), consume_batch()                   │
│  - invariants.rs: debug_assert_bounded_count!                               │
│  - Tests: loom, miri, proptest, MBT                                         │
└─────────────────────────────────────────────────────────────────────────────┘
```

### Why This Hierarchy Matters for Agents

When an AI coding agent modifies code, it needs to understand:

1. **Rules**: Project conventions (e.g., "use `Relaxed` ordering for metrics")
2. **Spec**: What invariants must be preserved (e.g., "count never exceeds capacity")
3. **Code**: How to implement changes that maintain the spec

**The problem**: Natural language specs are ambiguous. An agent might:
- Misinterpret "bounded count" as "non-negative" instead of "≤ capacity"
- Not realize a change violates single-writer ownership
- Introduce a race condition that breaks happens-before

**The solution**: Formalize the spec so violations are mechanically detectable.

## From Natural Language to Formal Methods

### The Spec Gap

Natural language specifications are prone to ambiguity:

```markdown
<!-- spec.md - Natural language -->
### INV-SEQ-01: Bounded Count
The number of items in the ring never exceeds capacity and is never negative.
```

Questions an agent might have:
- "Never negative" - does that mean `tail - head ≥ 0` or `len() ≥ 0`?
- "Never exceeds capacity" - is the check `<` or `≤`?
- When exactly must this hold - during operations or only between them?

### Formal Specification Removes Ambiguity

```tla
\* TLA+ - Mathematical precision
BoundedCount == 
    /\ tail >= head                    \* Non-negative count
    /\ (tail - head) <= Capacity       \* Never exceeds capacity
```

This is **machine-checkable**. TLC will explore all reachable states and report if `BoundedCount` ever becomes false.

### The TLA+ → Quint → Rust Pipeline

```
┌──────────────────────────────────────────────────────────────────────────┐
│  FORMAL SPEC (Mathematical Truth)                                        │
│                                                                          │
│  ┌─────────────────┐         ┌─────────────────┐                         │
│  │  RingSPSC.tla   │────────▶│  RingSPSC.qnt   │                         │
│  │                 │ manual  │                 │                         │
│  │  • Variables    │ transl. │  • var hd       │                         │
│  │  • Invariants   │         │  • boundedCount │                         │
│  │  • Actions      │         │  • step action  │                         │
│  └────────┬────────┘         └────────┬────────┘                         │
│           │                           │                                  │
│           ▼                           ▼                                  │
│      TLC Model                   quint verify                            │
│      Checker                  (Apalache or TLC)                          │
│                                                                          │
│  Exhaustively verifies spec is internally consistent                     │
└──────────────────────────────────────────────────────────────────────────┘
                                    │
                                    │  Invariants & Actions
                                    ▼
┌──────────────────────────────────────────────────────────────────────────┐
│  IMPLEMENTATION (Rust Code)                                              │
│                                                                          │
│  ┌─────────────────┐         ┌─────────────────┐                         │
│  │  quint_mbt.rs   │         │  property_tests │                         │
│  │                 │         │                 │                         │
│  │  Execute spec   │         │  Random inputs  │                         │
│  │  traces on      │         │  verify same    │                         │
│  │  real Ring<T>   │         │  invariants     │                         │
│  └────────┬────────┘         └────────┬────────┘                         │
│           │                           │                                  │
│           └───────────┬───────────────┘                                  │
│                       ▼                                                  │
│              Ring<T> Implementation                                      │
│                                                                          │
│  Verifies implementation satisfies spec invariants                       │
└──────────────────────────────────────────────────────────────────────────┘
```

## Concrete Example: Ring Buffer Verification

Let's trace a single invariant through the entire pipeline.

### 1. Natural Language Spec (spec.md)

```markdown
### INV-SEQ-01: Bounded Count
```
0 ≤ (tail - head) ≤ capacity
```
The number of items in the ring never exceeds capacity and is never negative.
```

### 2. TLA+ Formalization (RingSPSC.tla)

```tla
BoundedCount == 
    /\ tail >= head                    
    /\ (tail - head) <= Capacity       

\* Actions must preserve this invariant
ProducerWrite ==
    /\ (tail - head) < Capacity        \* Precondition: space available
    /\ tail' = tail + 1                \* Effect: increment tail
    /\ UNCHANGED <<head, ...>>
```

### 3. Quint Translation (RingSPSC.qnt)

> **Variable renaming**: Quint (≥ 0.30.0) reserves `head` and `tail` as built-in list operation
> names. The spec uses `hd` (for `head`) and `tl` (for `tail`).

```quint
val boundedCount: bool =
    tl >= hd and (tl - hd) <= CAPACITY

action producerWrite = all {
    (tl - hd) < CAPACITY,              // Precondition
    tl' = tl + 1,                      // Effect
    hd' = hd,                          // Unchanged
    // ...
}
```

### 4. Model-Based Test (quint_mbt.rs)

The current MBT approach uses [`quint-connect`](https://crates.io/crates/quint-connect) to
**automatically generate** simulation traces from the Quint spec and replay them against the
real `Ring<T>`, with **automatic state comparison** at every step. See
`model-based-testing-in-agentic-development.md` for a full description.

```rust
// quint-connect Driver: the spec IS the oracle — no manual invariant re-implementation needed.
// NOTE: Simplified — the actual driver tracks additional allocator state (buffer_capacity,
// initialized_slots, buffer_aligned, allocator_zst). See quint_mbt.rs for the full implementation.
impl Driver for RingSPSCDriver {
    type State = RingSPSCState;   // deserialized from ITF trace + constructed from real state

    fn step(&mut self, step: &Step) -> Result {
        switch!(step {
            producerWrite => {
                // Call the REAL Ring<T>:
                let mut reserved = self.ring.reserve(1)
                    .expect("Quint guard ensures space");
                reserved.as_mut_slice()[0] = MaybeUninit::new(self.produced);
                reserved.commit();
                self.produced += 1;
                self.items_produced += 1;
            },
            // ... other actions ...
        })
    }
}

// Three lines of Rust per test — traces auto-generated by quint run --mbt:
#[quint_run(spec = "tla/RingSPSC.qnt", main = "RingSPSC", seed = "1729", max_samples = 20)]
fn simulation_deep() -> impl Driver { RingSPSCDriver::default() }
```

### 5. Property Test (property_tests.rs)

```rust
proptest! {
    #[test]
    fn prop_bounded_count_ring(writes in 0usize..100) {
        let ring = Ring::<u64>::new(Config::default());
        let capacity = ring.capacity();
        
        for i in 0..writes {
            if let Some(mut r) = ring.reserve(1) {
                r.as_mut_slice()[0] = MaybeUninit::new(i as u64);
                r.commit();
            }
        }
        
        // INV-SEQ-01 from spec
        prop_assert!(ring.len() <= capacity);
    }
}
```

### 6. Runtime Assertion (invariants.rs)

```rust
macro_rules! debug_assert_bounded_count {
    ($count:expr, $capacity:expr) => {
        debug_assert!(
            $count <= $capacity,
            "INV-SEQ-01 violated: count {} > capacity {}",
            $count, $capacity
        )
    };
}
```

## Value for Agentic Development

### Problem: Agent-Introduced Regressions

Consider an AI agent asked to "optimize the reserve() function":

```rust
// Agent's "optimization" - looks reasonable!
fn reserve(&self, count: usize) -> Option<Reservation<T>> {
    // Removed the capacity check for "performance"
    let tail = self.tail.load(Relaxed);
    // ... proceed with reservation
}
```

This breaks INV-SEQ-01 but might pass simple unit tests.

### Solution: Formal Invariant Verification

With the formal pipeline:

1. **TLC/Quint verify** catches it immediately:
   ```
   Error: Invariant BoundedCount is violated.
   State 5: tail = 5, head = 0, Capacity = 4
   ```

2. **MBT tests** fail:
   ```
   assertion failed: check_bounded_count(capacity)
   produced=5, consumed=0, capacity=4
   ```

3. **Property tests** fail:
   ```
   proptest: prop_bounded_count_ring failed
   ring.len()=5 > capacity=4
   ```

The agent's change is rejected at multiple levels.

### Enabling Agent-Spec Awareness

Agents can be instructed to understand the spec hierarchy:

```markdown
<!-- copilot-instructions.md -->

## Invariants and Formal Methods

Before modifying lock-free code:
1. Read spec.md for relevant INV-* invariants
2. Check tla/RingSPSC.tla for formal definitions
3. Ensure changes preserve invariants
4. Run: cargo test --test property_tests --test quint_mbt --release

Invariants are NEVER optional. If an optimization violates an invariant,
the optimization is wrong, not the invariant.
```

## The Quint Ecosystem

Quint provides a modern toolchain for formal specification:

| Tool | Purpose | Usage |
|------|---------|-------|
| `quint typecheck` | Verify spec syntax/types | `quint typecheck RingSPSC.qnt` |
| `quint test` | Run embedded tests | `quint test RingSPSC.qnt --main=RingSPSC` |
| `quint run` | Random simulation | `quint run --max-steps=100` |
| `quint verify` | Model checking (Apalache default, TLC via `--backend=tlc`) | `quint verify --invariant=boundedCount` |

## Current Integration

```bash
# Spec-level verification (Quint land)
quint test RingSPSC.qnt --main=RingSPSC      # Spec tests
quint verify --invariant=safetyInvariant --backend=tlc   # Exhaustive check (TLC)
quint verify --invariant=safetyInvariant --backend=apalache  # Symbolic check (Apalache)

# Implementation verification (Rust land)
cargo test --test quint_mbt --features quint-mbt   # MBT traces
cargo test --test property_tests                    # Random inputs
```

### How MBT Bridges Spec and Implementation

The MBT driver doesn't duplicate implementation logic—it:

1. **Calls** real `Ring<T>` methods
2. **Tracks** state changes
3. Uses **automatic state comparison** (via `quint-connect`) to catch any divergence

See `model-based-testing-in-agentic-development.md` for the full architecture and code.

## Development Timeline

### ✅ Implemented: Automated Trace Generation via `quint-connect`

MBT traces are now **automatically generated** from the Quint spec using `quint-connect` v0.1.1.
The `#[quint_run]` macro invokes `quint run --mbt` to produce ITF traces with action metadata,
then replays each trace against the real `Ring<T>` implementation with state comparison at every step.

```bash
# Run all automated MBT tests (generates traces + replays against Ring<T>)
cargo test -p ringmpsc-rs --test quint_mbt --features quint-mbt --release

# With verbose trace output (shows actions & state at each step)
# NOTE: QUINT_VERBOSE is checked at *runtime* by the test driver, not by
# quint-connect (which uses compile-time option_env! and never sees the
# variable when installed from crates.io). Pass --nocapture so the test
# harness doesn't swallow stderr.
QUINT_VERBOSE=1 cargo test -p ringmpsc-rs --test quint_mbt --features quint-mbt --release -- --nocapture

# Reproduce a specific trace (e.g., a failing one) with a fixed seed.
# NOTE: Like QUINT_VERBOSE, QUINT_SEED is also checked at *runtime* by the
# test driver (runtime_seed()). It only affects the `simulation` test —
# the other two tests have seeds hardcoded in their #[quint_run] attribute.
QUINT_SEED=42 cargo test -p ringmpsc-rs --test quint_mbt --features quint-mbt --release
```

**Architecture:**
```
RingSPSC.qnt ──▶ quint run --mbt ──▶ ITF traces ──▶ quint-connect Driver ──▶ Ring<T>
 (spec)           (simulation)        (states +      (action dispatch +       (real impl)
                                      actions)       state comparison)
```

**Key design notes:**
- Uses `#[quint_run]` (not `#[quint_test]`) because `quint test` doesn't support `--mbt`
- Quint v0.30.0 required renaming `head`/`tail` → `hd`/`tl` (built-in name conflict)
- Multiple seeds (`1729`, `314159`, random) for diverse trace coverage
- The Quint spec's `run` declarations remain useful for standalone `quint test` verification

#### quint-connect: Value Over the Previous Approach

The earlier MBT implementation used **hand-crafted traces** — each test was a manually authored
sequence of `QuintAction` enums with Rust re-implementations of Quint invariants
(`check_bounded_count()`, `check_happens_before()`) called explicitly after every step. This had
two fundamental limitations: trace coverage was limited to what a human thought to write, and the
invariant checks could themselves diverge from the spec.

`quint-connect` eliminates both problems:

| Capability | Impact |
|---|---|
| **Automated trace generation** | `quint run --mbt` produces hundreds of random simulation traces from the spec — no hand-crafting needed |
| **Automatic state comparison** | After every step, expected state is deserialized from the ITF trace and compared to `State::from_driver()` — any divergence is caught, not just invariant violations |
| **Action dispatch via `switch!`** | `mbt::actionTaken` metadata tells the framework which action was taken; the driver dispatches automatically |
| **Eliminates redundant invariant code** | No need to re-code `boundedCount` or `happensBefore` in Rust — the spec IS the oracle |
| **Diverse coverage with minimal code** | Multiple `#[quint_run]` with different seeds explore different corners of the state space — 3 lines of Rust per test |

**The core shift:** the old approach asked *"Does Ring\<T\> satisfy invariants I re-implemented in
Rust?"* — the new approach asks *"Does Ring\<T\>'s state match exactly what the formal Quint model
says it should be after every action?"*. This is strictly stronger: it catches not only invariant
violations but also cases where the implementation produces a valid-but-wrong state (e.g., advancing
head by 2 instead of 1 while still satisfying `head ≤ tail`).

#### quint-connect MBT vs Deterministic Simulation Testing

See [model-based-testing-in-agentic-development.md §6](model-based-testing-in-agentic-development.md) for a detailed comparison of `quint-connect` MBT and Deterministic Simulation Testing (DST, as in FoundationDB/TigerBeetle/Antithesis). The key takeaway: MBT tests protocol correctness ("does the logic match the spec?") while DST tests concurrency correctness ("do all thread interleavings work?"). In ringmpsc-rs, the verification stack uses both paradigms complementarily — `quint-connect` MBT for spec conformance and Loom for exhaustive thread interleaving.

### Medium-Term: Agent-Driven Spec Evolution

AI agents could propose spec changes:

```
Agent: "To implement batch reservations, I need to relax INV-SEQ-01 
        to allow temporary over-allocation during reservation."

System: "Propose TLA+ amendment:"
        
        BatchBoundedCount ==
            /\ tail >= head
            /\ (tail - head) <= Capacity + MaxBatchSize  \* During reservation
            /\ committed => (tail - head) <= Capacity    \* After commit
        
Agent: "Running quint verify on amended spec..."
       "No violations found. Proceeding with implementation."
```

### Long-Term: Continuous Formal Verification

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                        CI/CD with Formal Methods                            │
│                                                                             │
│  1. Agent proposes code change                                              │
│                    │                                                        │
│                    ▼                                                        │
│  2. Extract affected invariants from git diff                               │
│                    │                                                        │
│                    ▼                                                        │
│  3. Run targeted formal verification                                        │
│     • quint verify --invariant=<affected>                                   │
│     • cargo test --test quint_mbt                                           │
│                    │                                                        │
│                    ▼                                                        │
│  4. Pass/Fail gates merge                                                   │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### Quint Ecosystem Roadmap

> **As of Quint 0.31.0 (2026-02-27)**. See [QUINT_0_31_UPGRADE.md](QUINT_0_31_UPGRADE.md) for upgrade notes. Items marked "Planned" may have shipped in later Quint releases.

| Feature | Status | Impact on Agentic Dev |
|---------|--------|----------------------|
| `quint typecheck` | ✅ Stable | Spec syntax validation |
| `quint test` | ✅ Stable | Embedded spec tests |
| `quint run` | ✅ Stable | Simulation traces |
| `quint verify` | ✅ Stable | Exhaustive verification |
| ITF trace export | ✅ Stable | Automated MBT |
| `quint-connect` | ✅ Stable (v0.1.1) | Rust integration |
| Language Server | 🔄 Developing | IDE support for agents |
| Property synthesis | 📋 Planned | Auto-generate invariants |

## Best Practices for Agentic Formal Methods

### 1. Spec-First Development

```markdown
Before implementing feature X:
1. Write INV-* invariants in spec.md
2. Formalize in TLA+/Quint
3. Run model checker
4. Then implement in Rust
5. Add MBT and property tests
```

### 2. Invariant Traceability

Every code change should reference its invariant:

```rust
// ring.rs
/// TLA+ Action: ProducerWrite
/// Preserves: INV-SEQ-01 (BoundedCount), INV-ORD-01 (Release semantics)
fn commit_internal(&self, count: usize) {
    debug_assert_bounded_count!(self.len() + count, self.capacity());
    // ...
}
```

### 3. Defense in Depth

```
┌─────────────────────────────────────────────────────────────────┐
│  Verification Layer Stack                                       │
├─────────────────────────────────────────────────────────────────┤
│  TLA+/Quint Model Checking    │ Spec correctness                │
│  Quint MBT (quint_mbt.rs)     │ Spec-implementation conformance │
│  Property Tests (proptest)    │ Random input coverage           │
│  Loom Tests                   │ Concurrency correctness         │
│  Miri Tests                   │ Memory safety                   │
│  debug_assert! macros         │ Runtime invariant checks        │
└─────────────────────────────────────────────────────────────────┘
```

### 4. Agent Instructions

```markdown
<!-- copilot-instructions.md -->

## Formal Verification Requirements

When modifying ringmpsc core:

1. **Identify affected invariants** in spec.md
2. **Check formal spec** in tla/RingSPSC.tla or tla/RingSPSC.qnt
3. **Run verification**:
       ```bash
       quint test tla/RingSPSC.qnt --main=RingSPSC
       cargo test --test quint_mbt --features quint-mbt --release
       cargo test --test property_tests --features stack-ring --release
       ```
4. **Never bypass invariants** - if a change requires violating an invariant, the invariant must be formally amended first
```

## Conclusion

Formal methods provide the missing link in agentic development:

| Without Formal Methods   | With Formal Methods            |
|--------------------------|--------------------------------|
| Specs are ambiguous      | Specs are mathematical         |
| Agents guess intent      | Agents verify against spec     |
| Regressions in PRs       | Violations caught mechanically |
| Manual review required   | Automated verification         |
| "It works on my machine" | "It satisfies the invariants"  |

The combination of:
- **TLA+/Quint** for formal specification
- **Model checking** for spec verification  
- **MBT** for implementation conformance
- **Property testing** for random coverage

Creates a verification pipeline where AI agents can confidently modify complex concurrent code knowing that invariant violations will be caught before merge.

With `quint-connect` v0.1.1 now providing stable Rust integration and automated trace generation from Quint simulations, this workflow is increasingly seamless—enabling agents to propose, verify, and implement changes to critical systems with mathematical confidence.
