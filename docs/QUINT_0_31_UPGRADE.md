# Quint 0.31.0 Upgrade: Impact on ringmpsc-rs Verification

This document describes the changes made to the ringmpsc-rs formal verification infrastructure following the [Quint 0.31.0 release](https://github.com/informalsystems/quint/releases/tag/v0.31.0) (2026-02-27), the technical benefits they bring, and the resulting architecture.

## Background

The ringmpsc SPSC ring buffer protocol is formally specified in both TLA+ (`RingSPSC.tla`) and Quint (`RingSPSC.qnt`). Prior to this upgrade, the verification workflow required:

- **Quint 0.30.0** with the TypeScript simulator backend for `quint run` and `quint test`
- **Standalone TLC** invoked via `java -jar tla2tools.jar` on the `.tla` file for exhaustive model checking
- **JDK 11**, which blocked both `quint verify` backends (Apalache and TLC) since Apalache requires JDK 17+
- **Two specification files** (`.tla` + `.qnt`) maintained in parallel for different toolchain paths

This created friction: the `.tla` file was the only path to exhaustive model checking, the `.qnt` file was used for simulation and MBT trace generation, and keeping them in sync was a manual process.

## What Changed in Quint 0.31.0

### Rust Backend is Now Default

`quint run` and `quint test` switched from the TypeScript evaluator to a native Rust evaluator compiled to a platform binary. Key characteristics:

- **~10× faster simulation**. The Rust evaluator uses MiMalloc as its allocator and streams JSON I/O to reduce memory allocations. For `RingSPSC.qnt` with default parameters, simulation throughput increased from ~400 traces/sec to ~3,900 traces/sec.
- **`--mbt` flag supported natively**. The `quint run --mbt` invocation that `quint-connect` uses to generate ITF traces now runs through the Rust backend, directly accelerating the model-based testing pipeline.
- **`--invariant` flag in `quint run`**. Invariants can be checked across thousands of random simulation traces without requiring a full model checker. This provides a fast feedback loop during development.
- **Better error diagnostics**. The Rust backend prints the seed and full trace on runtime errors and panics, and supports per-step `q::debug` diagnostics.

The TypeScript backend remains available via `--backend=ts` for cases requiring BigInt support (the Rust backend uses i64).

### TLC as a `quint verify` Backend

TLC (the TLA+ model checker) is now available via `quint verify --backend=tlc`. This:

- Translates the `.qnt` spec to TLA+ using Apalache internally
- Runs TLC for exhaustive state enumeration
- Returns results through the Quint CLI

This eliminates the need to maintain a separate `.tla` file and `.cfg` configuration for safety verification. The `.qnt` file becomes the single source of truth for simulation, testing, MBT trace generation, and exhaustive model checking.

### Other Relevant Changes

| Change | Impact |
|--------|--------|
| `quint test --backend=rust` | Embedded `run` test blocks execute via Rust backend |
| `quint repl --backend=rust` | Interactive exploration of specs at Rust speed |
| `--witnesses` flag in Rust backend | Witness extraction for counterexamples |
| `--n-traces` flag in Rust backend | Control number of generated traces |
| Per-step `q::debug` diagnostics | Richer tracing during spec development |
| Stack trace on runtime errors | Faster debugging of spec issues |

## Infrastructure Changes Made

### JDK 11 → 21 LTS

Both `quint verify` backends require Apalache, which requires JDK 17+. We installed JDK 21 (current LTS, supported through 2028) and added a `.java-version` file to `crates/ringmpsc/tla/` for contributor onboarding.

**Verification results with JDK 21:**

| Backend | Command | Result |
|---------|---------|--------|
| TLC | `quint verify --backend=tlc --invariant=safetyInvariant` | 955 states, 0 violations |
| TLC | `quint verify --backend=tlc --invariant=noDeadlock` | 955 states, 0 violations |
| Apalache | `quint verify --invariant=safetyInvariant` | 10-step symbolic check, no violations |

### Two-Backend Verification Strategy

We adopted a complementary two-backend approach:

| Backend | Approach | Strengths | Limitations |
|---------|----------|-----------|-------------|
| **TLC** (`--backend=tlc`) | Explicit enumeration of all reachable states | Complete coverage for bounded parameters; counterexample traces | State space explodes exponentially with parameter size |
| **Apalache** (default) | Symbolic model checking via SMT solver | Handles larger parameter values; finds deep bugs without full enumeration | Bounded by step depth, not exhaustive |

For `RingSPSC.qnt` with `CAPACITY=4, MAX_ITEMS=8`, TLC enumerates 955 distinct states at depth 20 in under 1 second. Apalache provides complementary symbolic coverage at larger parameter scales.

### `.tla` Retained for Liveness

The TLA+ spec (`RingSPSC.tla`) is retained specifically for the `EventuallyConsumed` liveness property:

```tla
EventuallyConsumed == items_produced = MaxItems ~> (head = tail)
```

The `~>` (leads-to) temporal operator has no Quint equivalent. For all safety verification (`BoundedCount`, `HappensBefore`, `SafetyInvariant`, `NoDeadlock`), the `.qnt` spec is the authoritative source. The `.tla` file is used only for liveness checking via standalone TLC.

### `.gitignore` for Apalache Artifacts

`quint verify` generates `_apalache-out/` directories containing detailed logs and SMT solver traces. A `.gitignore` was added to prevent these from being committed.

## Impact on Verification Pipeline

### Before (Quint 0.30.0 + JDK 11)

```
RingSPSC.tla ──→ standalone TLC ──→ exhaustive safety checking
                                      (manual java -jar invocation)

RingSPSC.qnt ──→ quint run (TypeScript) ──→ simulation (~400 traces/sec)
             ──→ quint test (TypeScript) ──→ embedded tests
             ──→ quint run --mbt (TypeScript) ──→ ITF traces ──→ quint-connect ──→ Ring<T>

quint verify: BLOCKED (JDK 11)
```

### After (Quint 0.31.0 + JDK 21)

```
RingSPSC.qnt ──→ quint verify --backend=tlc ──→ exhaustive safety (955 states)
             ──→ quint verify (Apalache)     ──→ symbolic safety checking
             ──→ quint run (Rust)            ──→ simulation (~3,900 traces/sec)
             ──→ quint run --invariant       ──→ simulation + invariant checking
             ──→ quint test (Rust)           ──→ embedded tests
             ──→ quint run --mbt (Rust)      ──→ ITF traces ──→ quint-connect ──→ Ring<T>

RingSPSC.tla ──→ standalone TLC ──→ liveness only (EventuallyConsumed)
```

The `.qnt` spec is now the single entry point for all safety verification paths. The `.tla` file's role is narrowed to liveness checking.

### Development Workflow

```
1. Edit Quint spec (RingSPSC.qnt)
   ↓
2. quint test (Rust backend, <1s) — spec-level sanity
   ↓
3. quint run --invariant=safetyInvariant (Rust, ~2.5s) — simulation + invariant check
   ↓
4. quint verify --backend=tlc (TLC, ~1s) — exhaustive safety
   ↓
5. quint verify (Apalache, ~4s) — symbolic complementary check
   ↓
6. cargo test --test quint_mbt (MBT, ~0.6s) — spec-to-implementation conformance
```

Total wall-clock time for the full pipeline: **under 10 seconds**.

## Impact on Model-Based Testing

The `quint-connect` integration (`quint_mbt.rs`) benefits directly:

1. **Faster trace generation**: `quint run --mbt` now runs through the Rust backend. The 3 MBT test functions (`simulation`, `simulation_deep`, `simulation_seed_variation`) complete in 0.58s total (release build).

2. **Native `--mbt` support**: The Rust backend handles `mbt::actionTaken` metadata natively, producing ITF traces with action names and nondeterministic picks embedded. No fallback to the TypeScript backend is needed.

3. **Reproducibility**: The Rust backend prints the seed on every run and on errors, making trace reproduction straightforward via `QUINT_SEED=<hex>`.

4. **No code changes required**: The `quint-connect` v0.1.1 crate, the `#[quint_run]` macro, and the `Driver`/`State` trait implementations required zero modifications. The upgrade is purely at the CLI layer.

## Trust Hierarchy (Updated)

```
┌─────────────────────────────────────────────────────────────────┐
│  HIGHEST TRUST: Quint / TLA+ Spec                               │
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
│  - Model-based testing via quint-connect (Rust backend traces)  │
│  - Property-based random input verification                     │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  BASE TRUST: Integration + Unit Tests                           │
│  - Exercises happy paths and edge cases                         │
└─────────────────────────────────────────────────────────────────┘
```

## Remaining Work

| Item | Status | Notes |
|------|--------|-------|
| CI workflow (`.github/workflows/quint.yml`) | Planned | `quint verify` (both backends) + MBT in GitHub Actions |
| `q::debug` diagnostics in `.qnt` spec | Planned | Per-step tracing for richer MBT debugging |
| MPSC channel spec (`RingMPSC.qnt`) | Planned | Multi-producer model using Quint's nondeterminism |
| Liveness in Quint | Blocked | `~>` not yet in Quint; `.tla` retained until supported |
| Deprecate `.tla` for safety | Ready | Blocked only on CI validation of `quint verify --backend=tlc` |

## References

- [Quint 0.31.0 Release Notes](https://github.com/informalsystems/quint/releases/tag/v0.31.0)
- [RingSPSC.qnt](../crates/ringmpsc/tla/RingSPSC.qnt) — Quint formal specification
- [RingSPSC.tla](../crates/ringmpsc/tla/RingSPSC.tla) — TLA+ specification (liveness only)
- [quint_mbt.rs](../crates/ringmpsc/tests/quint_mbt.rs) — Model-based test driver
- [FORMAL_VERIFICATION_WORKFLOW.md](FORMAL_VERIFICATION_WORKFLOW.md) — Full verification pipeline
- [quint-connect crate](https://crates.io/crates/quint-connect) — Quint ↔ Rust bridge
