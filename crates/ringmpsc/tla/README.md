# Formal Specification (Quint)

This directory contains the [Quint](https://quint-lang.org/) specifications for formal verification of the ringmpsc lock-free protocols.

> **History**: the spec started life as `RingSPSC.tla` (TLA+), was translated to Quint for the 0.31.0 toolchain upgrade, and the `.tla`/`.cfg` pair was retired once Quint 0.32.0 added the `leadsTo` temporal operator — the last TLA+ feature the project needed that Quint lacked. See [docs/QUINT_0_32_UPGRADE.md](../../../docs/QUINT_0_32_UPGRADE.md) for the migration details, and git history for the original TLA+ sources.

## Files

| File | Description |
|------|-------------|
| [RingSPSC.qnt](RingSPSC.qnt) | SPSC ring buffer spec: state, actions, safety invariants, embedded tests |
| [RingSPSCLiveness.qnt](RingSPSCLiveness.qnt) | Temporal (liveness) properties; imports `RingSPSC` (requires Quint ≥ 0.32.0) |
| [../tests/quint_mbt.rs](../tests/quint_mbt.rs) | Model-based test driver (ITF trace replay via `quint-connect`) |

## Design Decisions

### Unbounded Integers (not u64 wrap-around)

The Rust implementation uses `u64` sequence numbers to prevent ABA problems. At 10 billion messages/second, wrap-around takes ~58 years.

The Quint model uses unbounded `int` instead of modeling wrap-around arithmetic because:

1. **Invariants don't depend on overflow** - Bounded count and monotonicity hold regardless of number representation
2. **Finite state space** - the model checker explores states bounded by `MAX_ITEMS` anyway
3. **Separation of concerns** - Wrap-around correctness is tested empirically via Loom

### Liveness Lives in a Separate Module

`RingSPSCLiveness.qnt` uses `leadsTo` and `weakFair`, which require Quint ≥ 0.32.0. Keeping the temporal properties in their own module (importing `RingSPSC`) means the safety spec — the file the `quint-connect` MBT pipeline loads — remains usable with Quint 0.31.x toolchains.

### Fairness Is Part of the Property

`eventuallyConsumed` ("once everything is produced, the ring eventually drains") is only provable under a fairness assumption: without it, a behavior that stutters forever with items still in the ring is a legal counterexample. Quint has no `WF_vars` spec-level syntax, so the weak-fairness assumption is stated *inside* the property:

```
temporal eventuallyConsumed =
    consumerFair.implies((items_produced == MAX_ITEMS).leadsTo(hd == tl))
```

This is stronger than what the retired TLA+ setup ever checked: there, `EventuallyConsumed` was commented out of the TLC configuration ("requires fairness assumptions") and the behavior spec declared no fairness, so the property had never actually been verified. Removing the fairness antecedent reproduces the expected counterexample; with it, TLC verifies the property over the complete state space.

### Refinement Mapping

| Quint Action | Rust Function | Spec Invariant |
|--------------|---------------|----------------|
| `producerReserveFast` | `ring.rs: reserve()` fast path | INV-ORD-01 |
| `producerRefreshCache` | `ring.rs: reserve()` slow path | INV-SW-01 |
| `producerWrite` | `ring.rs: commit_internal()` | INV-SEQ-01, INV-ORD-01 |
| `consumerRefreshCache` | `ring.rs: consume_batch()` slow path | INV-SW-02 |
| `consumerAdvance` | `ring.rs: advance()`, `consume_batch()` | INV-SEQ-01, INV-ORD-02 |

## Installing Quint

```bash
# Via npm (recommended)
npm install -g @informalsystems/quint

# Verify installation (must be ≥ 0.32.0 for the liveness module)
quint --version
```

> **Quint 0.32.0 highlights** (2026-03-31):
> - **`leadsTo` temporal operator** (TLA+ `~>`) — enables liveness checking from `.qnt` files
> - TLC backend fixes (parallel execution, Apalache availability check)
> - Windows compatibility for the Rust evaluator; better error propagation on evaluator startup
>
> **Quint 0.31.0 highlights** (2026-02-27):
> - **Rust backend is the default** for `quint run` and `quint test` — ~10× faster simulation
> - **TLC available as a backend** for `quint verify --backend=tlc` — exhaustive model checking directly from `.qnt` files
> - `--mbt`, `--invariants`, `--n-traces`, `--witnesses` flags all supported by the Rust backend

## Running Quint

```bash
cd crates/ringmpsc/tla

# Typecheck the specs
quint typecheck RingSPSC.qnt
quint typecheck RingSPSCLiveness.qnt

# Run simulation (Rust backend, default since 0.31.0)
quint run RingSPSC.qnt --main=RingSPSC --max-steps=100

# Simulation with invariant checking
quint run RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant

# Run the 18 embedded tests. Their names carry a `Test` suffix because
# `quint test`'s default --match only selects names containing "Test".
quint test RingSPSC.qnt --main=RingSPSC

# Exhaustive safety checking via TLC backend (requires JDK 21+, see .java-version)
# Explores ALL reachable states — 955 states for default parameters.
quint verify RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant --backend=tlc
quint verify RingSPSC.qnt --main=RingSPSC --invariant=noDeadlock --backend=tlc

# Exhaustive liveness checking (requires Quint ≥ 0.32.0, JDK 21+)
quint verify RingSPSCLiveness.qnt --main=RingSPSCLiveness --temporal=eventuallyConsumed --backend=tlc

# Symbolic model checking via Apalache backend (requires JDK 21+)
# Complementary to TLC — works well at larger parameter values.
quint verify RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant

# Explicitly use the TypeScript backend (slower, supports BigInts)
quint run RingSPSC.qnt --main=RingSPSC --backend=ts
```

## Exhaustive Model Checking: `quint verify --backend=tlc`

Since Quint 0.31.0, the TLC model checker can be invoked directly from a `.qnt` file, and since 0.32.0 that includes temporal properties:

```bash
quint verify RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant --backend=tlc
quint verify RingSPSCLiveness.qnt --main=RingSPSCLiveness --temporal=eventuallyConsumed --backend=tlc
```

The `.qnt` specs are the **single source of truth** for simulation, testing, MBT trace generation, exhaustive safety checking, *and* liveness checking. There is no separate `.tla`/`.cfg` pair to keep in sync.

> **Prerequisite**: JDK 21+ is required for both `quint verify` backends (see [`.java-version`](.java-version)). Install via `brew install openjdk@21`. Quint downloads and manages the Apalache/TLC distribution itself.

### Two-Backend Strategy

| Backend | Command | Approach | Best for |
|---------|---------|----------|----------|
| **TLC** | `quint verify --backend=tlc` | Explicit enumeration of all reachable states | Small parameters (CAPACITY ≤ 8); exhaustive guarantees; temporal properties |
| **Apalache** | `quint verify` (default) | Symbolic model checking via SMT solver | Larger parameters; finds deep bugs without full state enumeration |

Both backends use the same `.qnt` specs as input. Use them together for complementary coverage.

## Adjusting Model Parameters

`CAPACITY` and `MAX_ITEMS` are `val`s at the top of [RingSPSC.qnt](RingSPSC.qnt):

| Parameter | Default | Effect |
|-----------|---------|--------|
| `CAPACITY` | 4 | Ring buffer size |
| `MAX_ITEMS` | 8 | Total items to produce (bounds state space) |

**Tradeoffs:**
- Larger values → more thorough checking, exponentially more states
- `CAPACITY=4, MAX_ITEMS=8` checks 955 states in under a second
- `CAPACITY=8, MAX_ITEMS=16` checks ~100K states in minutes

## Model-Based Testing

The [quint_mbt.rs](../tests/quint_mbt.rs) driver uses `quint-connect` v0.1.1 to
automatically generate traces from the Quint spec and replay them against the real `Ring<T>`:

```bash
# Run model-based tests (automated trace generation via quint run --mbt)
cargo test -p ringmpsc-rs --test quint_mbt --features quint-mbt --release
```

The driver:
1. `quint-connect` invokes `quint run --mbt` to simulate the spec and produce ITF traces
2. Since Quint 0.31.0, the Rust backend handles `--mbt` natively — faster trace generation
3. Each trace (sequence of named actions + state snapshots) is deserialized automatically
4. The `switch!` macro dispatches each action to the corresponding `Ring<T>` operation
5. After each step, `quint-connect` compares the driver's state with the spec's expected state

## Quint ↔ TLA+ Mapping

> For a comprehensive translation reference including common pitfalls (reserved names, `int` semantics, `UNCHANGED` handling), see [docs/QUINT_TRANSLATION.md](../../../../docs/QUINT_TRANSLATION.md). The table below is a summary of the most common constructs.

| TLA+ | Quint | Notes |
|------|-------|-------|
| `VARIABLE x` | `var x: int` | Quint requires types |
| `x' = expr` | `x' = expr` | Same syntax |
| `\/ A \/ B` | `any { A, B }` | Nondeterministic choice |
| `/\ A /\ B` | `all { A, B }` | Conjunction |
| `[Next]_vars` | `step` action | Stuttering in `run` |
| `~>` (leads-to) | `leadsTo` | Since Quint 0.32.0 |
| `WF_v(A)` | `weakFair(A, v)` | State it inside the property, not the spec |

## Future Work

- [ ] Add `quint verify --backend=tlc` (safety + liveness) + MBT to CI (`.github/workflows/quint.yml`) when CI/CD is set up
- [ ] Add MPSC channel specification (`RingMPSC.qnt`) modeling multiple producers
- [ ] Loom trace export → Quint verification
- [x] ~~Add liveness checking with fairness constraints~~ (`RingSPSCLiveness.qnt`, Quint 0.32.0)
- [x] ~~Retire `RingSPSC.tla`/`RingSPSC.cfg`~~ (Quint 0.32.0 `leadsTo` closed the gap)
- [x] ~~Translate `RingSPSC.tla` to `RingSPSC.qnt` for Quint tooling~~
- [x] ~~Implement `quint-connect` driver for model-based testing~~
- [x] ~~ITF trace parsing for automated test generation~~ (via `quint-connect` v0.1.1)
- [x] ~~TLC model checking via `.qnt` file~~ (via `quint verify --backend=tlc`, Quint 0.31.0)
- [x] ~~Rust backend for faster simulation~~ (default since Quint 0.31.0)
