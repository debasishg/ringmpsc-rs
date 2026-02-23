# Evolution of Model-Based Testing with `quint-connect`

[Quint](https://quint-lang.org/) is a modern specification language designed as an accessible alternative to TLA+, combining TLA+'s rigorous state-machine semantics with a TypeScript-inspired syntax that feels natural to working programmers. Its toolchain — `quint typecheck`, `quint run` (simulation), `quint test`, and `quint verify` (symbolic model checking via Apalache) — enables formal verification workflows without leaving a familiar development environment. The [`quint-connect`](https://crates.io/crates/quint-connect) crate bridges this ecosystem into Rust: it invokes the Quint CLI to generate simulation traces in ITF (Informal Trace Format), then replays those traces against a user-defined Rust `Driver`, automatically comparing the implementation's state with the spec's expected state at every step. This tight integration means Rust projects can adopt model-based testing with minimal boilerplate — a `#[quint_run]` attribute, a `Driver` impl, and a `State` struct are all that's needed to connect a formal specification to a real implementation.

This document traces the evolution of Model-Based Testing (MBT) in the `ringmpsc` crate — from hand-crafted trace sequences with manually re-implemented invariants, to fully automated trace generation and state comparison powered by `quint-connect`.

---

## 1. `quint-connect` Integration Details

[Quint](https://quint-lang.org/) is a modern specification language with TLA+ semantics and TypeScript-like syntax. It provides a formal model of a system's state transitions that can be **machine-checked** (`quint verify`) and **simulated** (`quint run`).

`quint-connect` is a Rust crate that bridges Quint specifications and Rust implementations. It provides:

| Capability | Mechanism |
|---|---|
| **Trace generation** | Invokes `quint run --mbt` to simulate the spec and produce ITF (Informal Trace Format) traces |
| **Action dispatch** | The `switch!` macro routes each trace step to the corresponding Rust method call |
| **Automatic state comparison** | After every step, deserializes the expected state from the ITF trace and compares it with the driver's actual state via `State::from_driver()` |
| **Proc-macro integration** | `#[quint_run]` and `#[quint_test]` attributes generate test functions that orchestrate the full simulate → replay → compare cycle |

The architecture is straightforward:

```text
RingSPSC.qnt ──▶ quint run --mbt ──▶ ITF traces ──▶ quint-connect Driver ──▶ Ring<T>
 (spec)           (simulation)        (states +      (action dispatch +       (real impl)
                                       actions)       state comparison)
```

This fits naturally into the project's multi-layered verification strategy (see also `self-verified-pipeline.md` and `FORMAL_METHODS_AGENTIC_DEVELOPMENT.md`):

| Layer | Tool | What It Catches |
|-------|------|-----------------|
| Formal Spec | TLA+ / Quint | Logical errors in protocol design |
| Model-Based Testing | `quint-connect` (`quint_mbt.rs`) | Spec ↔ implementation divergence |
| Property Testing | proptest | Random input violations |
| Concurrency Testing | Loom | Data races, ordering bugs |
| Memory Safety | Miri | Undefined behavior |

---

## 2. Initial Strategy: Hand-Crafted Traces and Explicit Verification

The first MBT approach was **manual**. Each test was an explicitly authored sequence of action enums, and invariant checks were re-implemented in Rust and called after every step.

### How It Worked

Traces were hand-crafted sequences of action enums:

```rust
// Simplified illustration of the old approach

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

Invariant checks were **re-implemented in Rust** and called explicitly after every action:

```rust
fn check_safety_invariant(sut: &RingSUT, capacity: u64) -> bool {
    // INV-SEQ-01: Bounded Count — re-implemented in Rust
    let bounded = (sut.produced - sut.consumed) <= capacity;
    // INV-ORD-03: Happens-Before — re-implemented in Rust
    let happens_before = sut.consumed <= sut.produced;
    bounded && happens_before
}
```

Tests were then specific, hand-authored scenarios targeting edge cases:

```rust
#[test]
fn test_cache_refresh_scenario() {
    let trace = vec![
        ProducerWrite, ProducerWrite, ProducerWrite, ProducerWrite,
        ConsumerAdvance, ConsumerAdvance,
        ProducerRefreshCache,
        ProducerWrite, ProducerWrite,
    ];
    execute_trace(&trace, 2).expect("Should pass");
}
```

### Limitations

This approach had two fundamental problems, as documented in `FORMAL_METHODS_AGENTIC_DEVELOPMENT.md`:

1. **Trace coverage was limited to human imagination.** Only the specific scenarios a developer thought to write were tested. Subtle interleavings — like a producer cache refresh immediately followed by a consumer advance — might never appear.

2. **Invariant checks could themselves diverge from the spec.** The Rust re-implementations of `boundedCount` and `happensBefore` were *separate code* from the Quint spec. A typo or misunderstanding in the Rust check would silently pass incorrect states.

The old approach asked: *"Does Ring\<T\> satisfy invariants I re-implemented in Rust?"*

---

## 3. Evolved Strategy: Automated Trace Generation with `quint-connect`

The new approach eliminates both limitations by making the **Quint spec itself the oracle**. Instead of re-implementing invariants in Rust, we let `quint-connect` compare the full state at every step.

The new approach asks: *"Does Ring\<T\>'s state match **exactly** what the formal Quint model says it should be after every action?"*

This is strictly stronger: it catches not only invariant violations but also cases where the implementation produces a valid-but-wrong state (e.g., advancing `head` by 2 instead of 1 while still satisfying `head ≤ tail`).

### 3.1 The State: Deserialized from Quint ITF Traces

The `RingSPSCState` struct (defined in `crates/ringmpsc/tests/quint_mbt.rs`) mirrors the five Quint state variables. It is both deserialized from ITF trace output **and** constructed from the driver's internal tracking:

```rust
// crates/ringmpsc/tests/quint_mbt.rs

#[derive(Eq, PartialEq, Deserialize, Debug)]
struct RingSPSCState {
    #[serde(rename = "hd")]
    head: i64,                // Quint variable: hd
    #[serde(rename = "tl")]
    tail: i64,                // Quint variable: tl
    cached_head: i64,
    cached_tail: i64,
    items_produced: i64,
}

impl State<RingSPSCDriver> for RingSPSCState {
    fn from_driver(driver: &RingSPSCDriver) -> quint_connect::Result<Self> {
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

> **Note:** Quint v0.30.0 reserves `head` and `tail` as built-in list operations, so the spec uses `hd`/`tl`. The `#[serde(rename)]` attributes bridge this naming gap transparently.

After each step, `quint-connect` compares the two **automatically**:

```text
Expected state from Quint spec:
  RingSPSCState { head: 2, tail: 4, cached_head: 0, ... }

Actual state from Ring<T> driver:
  RingSPSCState { head: 1, tail: 4, cached_head: 0, ... }
                  ^^^^^^ divergence detected
```

No hand-written invariant check is needed — any state mismatch is immediately flagged.

### 3.2 The Driver: Connecting Ring\<T\> to Quint Actions

The `RingSPSCDriver` (in `crates/ringmpsc/tests/quint_mbt.rs`) wraps the real `Ring<u64>` and maintains abstract state tracking that mirrors Quint's variables:

```rust
// crates/ringmpsc/tests/quint_mbt.rs

struct RingSPSCDriver {
    ring: Ring<u64>,         // The REAL ring buffer
    consumed: u64,           // mirrors Quint `hd`
    produced: u64,           // mirrors Quint `tl`
    cached_head: u64,        // mirrors Quint `cached_head`
    cached_tail: u64,        // mirrors Quint `cached_tail`
    items_produced: u64,     // mirrors Quint `items_produced`
}

impl Default for RingSPSCDriver {
    fn default() -> Self {
        // capacity = 2^2 = 4, matching CAPACITY = 4 in RingSPSC.qnt
        let config = Config::new(2, 1, false);
        Self {
            ring: Ring::new(config),
            consumed: 0,
            produced: 0,
            cached_head: 0,
            cached_tail: 0,
            items_produced: 0,
        }
    }
}
```

The `Driver::step` implementation uses `quint-connect`'s `switch!` macro to dispatch each action from the trace to the corresponding `Ring<T>` operation:

```rust
// crates/ringmpsc/tests/quint_mbt.rs

impl Driver for RingSPSCDriver {
    type State = RingSPSCState;

    fn step(&mut self, step: &Step) -> quint_connect::Result {
        let verbose = is_verbose();
        let action = &step.action_taken;

        switch!(step {
            init => {
                *self = Self::default();
            },

            producerReserveFast => {
                // Guard-only action — no state mutation in Quint.
            },

            producerRefreshCache => {
                // Quint: cached_head' = head
                self.cached_head = self.consumed;
            },

            producerWrite => {
                // Quint: tail' = tail + 1, items_produced' = items_produced + 1
                // Drive the real Ring to verify conformance:
                let mut reserved = self.ring.reserve(1)
                    .expect("reserve(1) should succeed: Quint guard ensures space");
                reserved.as_mut_slice()[0] = MaybeUninit::new(self.produced);
                reserved.commit();
                self.produced += 1;
                self.items_produced += 1;
            },

            consumerReadFast => {
                // Guard-only action — no state mutation.
            },

            consumerRefreshCache => {
                // Quint: cached_tail' = tail
                self.cached_tail = self.produced;
            },

            consumerAdvance => {
                // Quint: head' = head + 1
                let consumed = self.ring.consume_up_to(1, |_item| {});
                assert_eq!(consumed, 1,
                    "consume_up_to(1) should return 1: Quint guard ensures head < tail");
                self.consumed += 1;
            },
        })
    }
}
```

Key observations:

- **Producer and consumer actions call the *real* `Ring<T>`** — `reserve(1)`, `commit()`, `consume_up_to(1)` — not mock implementations.
- **Guard-only actions** (`producerReserveFast`, `consumerReadFast`) model precondition checks in the spec but require no state mutation; the Quint simulator already verified the guard during trace generation.
- **No invariant re-implementation** — `boundedCount` and `happensBefore` are verified implicitly through state comparison at every step.

### 3.3 Automated Trace Generation: `#[quint_run]` and Seed-Based Simulation

This is the most impactful change. Traces are no longer hand-crafted — they are **automatically generated** by the Quint simulator.

The `#[quint_run]` proc-macro:

1. Invokes `quint run --mbt` to simulate the spec and produce ITF traces
2. Each ITF trace contains state snapshots + `mbt::actionTaken` metadata
3. Deserializes each step and calls `Driver::step()` with the action name
4. After each step, compares `State::from_driver()` with the expected ITF state

> **Why `#[quint_run]` and not `#[quint_test]`?** Only `quint run` supports the `--mbt` flag needed to embed `mbt::actionTaken` metadata in ITF traces. `quint test` does not support this flag. The Quint spec's `run` declarations remain useful for standalone `quint test` verification of the spec itself.

#### Default Random Simulation

The primary simulation test uses a **runtime seed** so that `QUINT_SEED` can be set at runtime for reproducibility:

```rust
// crates/ringmpsc/tests/quint_mbt.rs

#[test]
fn simulation() {
    let driver = RingSPSCDriver::default();
    let seed = runtime_seed();
    let config = quint_connect::runner::Config {
        test_name: "simulation".to_string(),
        gen_config: quint_connect::runner::RunConfig {
            spec: "tla/RingSPSC.qnt".to_string(),
            main: Some("RingSPSC".to_string()),
            init: None,
            step: None,
            max_samples: None,
            max_steps: None,
            seed,
        },
    };
    if let Err(err) = quint_connect::runner::run_test(driver, config) {
        panic!("{}", err);
    }
}
```

The `runtime_seed()` helper works around a `quint-connect` limitation where `option_env!("QUINT_SEED")` is evaluated at compile time of the library crate (always `None` from crates.io). Instead, it checks the environment variable at runtime:

```rust
// crates/ringmpsc/tests/quint_mbt.rs

fn runtime_seed() -> String {
    std::env::var("QUINT_SEED").unwrap_or_else(|_| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before epoch")
            .subsec_nanos();
        format!("0x{:x}", nanos)
    })
}
```

#### Deep Exploration with Fixed Seeds

Additional tests use fixed seeds and higher sample counts for broader, reproducible coverage:

```rust
// crates/ringmpsc/tests/quint_mbt.rs

/// Broader state-space exploration via more samples (max_samples = 20).
/// Each sample is one independent simulation trace — a random walk through
/// the spec's state space. Fixed seed (1729) makes all 20 traces reproducible.
#[quint_run(spec = "tla/RingSPSC.qnt", main = "RingSPSC", seed = "1729", max_samples = 20)]
fn simulation_deep() -> impl Driver {
    RingSPSCDriver::default()
}

/// Another seed for broader coverage across CI runs.
#[quint_run(spec = "tla/RingSPSC.qnt", main = "RingSPSC", seed = "314159")]
fn simulation_seed_variation() -> impl Driver {
    RingSPSCDriver::default()
}
```

Each `#[quint_run]` invocation is just **3 lines of Rust** — the framework handles trace generation, deserialization, action dispatch, and state comparison automatically.

#### Verbose Tracing

The `is_verbose()` helper enables detailed trace output at runtime — invaluable for debugging failing traces:

```rust
// crates/ringmpsc/tests/quint_mbt.rs

fn is_verbose() -> bool {
    static VERBOSE: OnceLock<bool> = OnceLock::new();
    *VERBOSE.get_or_init(|| {
        std::env::var("QUINT_VERBOSE")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false)
    })
}
```

When enabled, each step prints the action name and resulting state to stderr:

```text
  [init] head=0 tail=0 cached_head=0 cached_tail=0 items_produced=0
  [producerWrite] tail=1 items_produced=1
  [producerWrite] tail=2 items_produced=2
  [consumerRefreshCache] cached_tail <- tail = 2
  [consumerAdvance] head=1
  [producerRefreshCache] cached_head <- head = 1
  ...
```

---

## 4. Running the Tests

### Prerequisites

```bash
# Install Quint CLI
npm install -g @informalsystems/quint

# Verify installation
quint --version
```

### Spec-Level Verification (Quint)

```bash
cd crates/ringmpsc/tla

# Typecheck the spec
quint typecheck RingSPSC.qnt

# Run the spec's own embedded tests
quint test RingSPSC.qnt --main=RingSPSC

# Exhaustive model checking via Apalache backend
quint verify RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant
```

### Implementation-Level MBT (Rust + `quint-connect`)

```bash
# Run all quint-connected tests (simulation + fixed-seed variants)
cargo test -p ringmpsc-rs --test quint_mbt --features quint-mbt --release

# With verbose trace output (shows actions & state at each step)
# NOTE: QUINT_VERBOSE is checked at *runtime* by the test driver, not by
# quint-connect (which uses compile-time option_env! and never sees the
# variable when installed from crates.io). Pass --nocapture so the test
# harness doesn't swallow stderr.
QUINT_VERBOSE=1 cargo test -p ringmpsc-rs --test quint_mbt \
    --features quint-mbt --release -- --nocapture

# Reproduce a specific failing trace with a fixed seed
# NOTE: QUINT_SEED only affects the `simulation` test. The `simulation_deep`
# and `simulation_seed_variation` tests have seeds hardcoded in their
# #[quint_run] attributes.
QUINT_SEED=42 cargo test -p ringmpsc-rs --test quint_mbt \
    --features quint-mbt --release

# Run alongside the full verification stack
cargo test -p ringmpsc-rs --test property_tests --features stack-ring --release
cargo test -p ringmpsc-rs --features loom --test loom_tests --release
cargo +nightly miri test -p ringmpsc-rs --test miri_tests
```

---

## 5. Comparison: Before and After

| Dimension | Hand-Crafted Traces (Before) | `quint-connect` Automated (After) |
|---|---|---|
| **Trace source** | Human-authored action sequences | `quint run --mbt` generates random simulation traces from the spec |
| **Coverage** | Limited to scenarios a developer imagined | Diverse interleavings across the full state space |
| **Oracle** | Re-implemented invariants in Rust (`check_bounded_count()`, `check_happens_before()`) | The Quint spec itself — full state comparison at every step |
| **Divergence detection** | Only catches invariant violations the Rust checks encode | Catches **any** state divergence, including valid-but-wrong states |
| **Maintenance** | Update traces and invariant checks when spec changes | Update only the `Driver::step` dispatch — traces regenerate automatically |
| **Lines of Rust per test** | 10–30 (trace + assertions) | 3 (`#[quint_run]` attribute + driver constructor) |
| **Reproducibility** | Deterministic (hardcoded sequences) | Seed-based (`QUINT_SEED` env var or fixed `seed = "..."` in attribute) |

---

## 6. `quint-connect` MBT vs Deterministic Simulation Testing

`quint-connect` MBT and Deterministic Simulation Testing (DST, as in FoundationDB / TigerBeetle / Antithesis) share a surface similarity — both replay deterministic sequences — but differ fundamentally:

| Dimension | DST | `quint-connect` MBT |
|---|---|---|
| **What executes** | The real system under a controlled scheduler that captures all nondeterminism | The Quint simulator generates traces; the Rust driver replays actions sequentially |
| **Oracle** | Assertions/invariants embedded in the code (you write them) | The formal spec itself (state comparison is automatic) |
| **Concurrency** | Primary strength — finds races, deadlocks, ordering bugs | Does not test concurrency (that's loom's job) |
| **Bug class: logic errors** | Can miss if no assertion covers it | Catches any state divergence from spec |
| **Bug class: concurrency** | Primary strength | Not tested |
| **Abstraction level** | System-level (real threads, real I/O) | Protocol-level (abstract state transitions) |

In `ringmpsc-rs`, the verification stack uses **both paradigms** complementarily:

```text
quint-connect MBT     → "Does the logic match the spec?"        (protocol correctness)
Property tests        → "Do random inputs preserve invariants?"  (input coverage)
Loom tests            → "Do all thread interleavings work?"      (≈ mini-DST for atomics)
Miri tests            → "Is there undefined behavior?"           (memory safety)
```

A system can pass MBT perfectly (correct protocol logic) and still fail under Loom (race condition in the implementation). The two approaches answer orthogonal questions — which is why the project runs **all layers** in CI.

---

## 7. Summary

The migration from hand-crafted MBT to `quint-connect` automated MBT delivered three key improvements:

1. **The spec is the oracle** — no redundant invariant code in Rust that can silently diverge from the Quint model.
2. **Traces are auto-generated** — the Quint simulator explores diverse action interleavings that a human might never write, and multiple seeds (`1729`, `314159`, random) ensure broad coverage.
3. **State comparison is automatic** — any divergence between the `Ring<T>` implementation and the formal model is caught immediately, at every step, with a clear diff.

The result is a 3-line-per-test MBT setup that provides stronger guarantees than the previous approach while requiring less maintenance — a foundation for confident evolution of lock-free ring buffer code.
