# Quint 0.32.0 Upgrade: Liveness in Quint, TLA+ Retired

> **Last updated**: 2026-07-18 | **Quint**: 0.32.0 | **JDK**: 21 LTS

This document describes the changes made following the [Quint 0.32.0 release](https://github.com/informalsystems/quint/releases/tag/v0.32.0) (2026-03-31). It completes the migration started in [QUINT_0_31_UPGRADE.md](QUINT_0_31_UPGRADE.md): the `.qnt` specs are now the *only* formal artifacts — the `RingSPSC.tla`/`RingSPSC.cfg` pair is retired.

## What Changed in Quint 0.32.0

| Change | Impact |
|--------|--------|
| **`leadsTo` temporal operator** ([PR #1932](https://github.com/informalsystems/quint/pull/1932)) | The TLA+ `~>` operator — the one feature that forced retention of `RingSPSC.tla` |
| TLC backend fixes | Parallel execution issues resolved; Apalache availability verified before execution |
| Windows compatibility for the Rust evaluator | Relevant for a future CI platform matrix |
| Better error propagation on evaluator startup | Faster debugging when the Rust backend fails to launch |

No breaking changes are documented in the release. Requires Apalache ≥ 0.56.1 for `leadsTo` transpilation (Quint manages its Apalache distribution itself).

## The Finding: Liveness Was Never Actually Checked

The 0.31.0 upgrade retained `RingSPSC.tla` solely for the liveness property:

```
EventuallyConsumed == items_produced = MaxItems ~> (head = tail)
```

Reviewing that setup for this upgrade surfaced a latent gap:

1. **The property was disabled.** `RingSPSC.cfg` had the `PROPERTIES EventuallyConsumed` entry commented out, with the note "requires fairness assumptions."
2. **The spec declared no fairness.** The behavior spec was `Spec == Init /\ [][Next]_vars` — no `WF_vars` anywhere. Weak fairness existed only as a prose comment above the property.
3. **The property was unprovable as written.** Without fairness, a behavior that stutters forever after producing `MaxItems` items is a legal counterexample.

So the `.tla` file was being kept alive for a property that had never been verified. The Quint port fixes this rather than just translating it.

## Changes Made

### 1. New `RingSPSCLiveness.qnt`

Temporal properties live in a new module that imports `RingSPSC`:

```
temporal consumerFair = weakFair(consumerAdvance, hd)

temporal eventuallyConsumed =
    consumerFair.implies((items_produced == MAX_ITEMS).leadsTo(hd == tl))
```

Design points:

- **Fairness is part of the property.** Quint has no `WF_vars` spec-level syntax; per the `leadsTo` PR discussion, fairness assumptions belong in the property itself. `weakFair(consumerAdvance, hd)` uses `hd` as the witness variable — `consumerAdvance` always changes it, so a non-stuttering advance is exactly `<<consumerAdvance>>_hd`.
- **Separate module keeps the safety spec 0.31-compatible.** `RingSPSC.qnt` (which the `quint-connect` MBT pipeline loads) uses no 0.32-only features, so contributors on Quint 0.31.x lose only liveness checking, not the whole pipeline.

### 2. `RingSPSC.tla` and `RingSPSC.cfg` Retired

Deleted; available in git history. All references across docs, agent instructions, and skills were updated to the Quint commands.

### 3. Embedded Test Runs Renamed (`*Test` suffix)

A second latent gap found during verification: `quint test`'s default `--match` only selects `run` names containing **"Test"**. None of the 18 embedded runs matched, so the documented `quint test RingSPSC.qnt --main=RingSPSC` invocation had been silently running **zero** tests — on 0.31.0 as well. All 18 runs now carry a `Test` suffix (`initSatisfiesInvariantTest`, `fillToCapacityTest`, …) and are picked up by the default match.

## Verification Results (Quint 0.32.0, JDK 21)

| Check | Command | Result |
|-------|---------|--------|
| Safety (exhaustive) | `quint verify RingSPSC.qnt --invariant=safetyInvariant --backend=tlc` | 955 states, 0 violations |
| No deadlock (exhaustive) | `quint verify RingSPSC.qnt --invariant=noDeadlock --backend=tlc` | 955 states, 0 violations |
| **Liveness (exhaustive)** | `quint verify RingSPSCLiveness.qnt --temporal=eventuallyConsumed --backend=tlc` | **955 states, 0 violations — first-ever check of this property** |
| Liveness without fairness (control) | same, property minus the `consumerFair` antecedent | Counterexample found, as expected — confirms the fairness assumption is load-bearing |
| Embedded tests | `quint test RingSPSC.qnt --main=RingSPSC` | 18 passing (0.31.0 and 0.32.0) |
| Simulation | `quint run RingSPSC.qnt --invariant=safetyInvariant` | No violation |
| MBT conformance | `cargo test -p ringmpsc-rs --test quint_mbt --features quint-mbt --release` | All traces conform |

## Toolchain Notes

- Install/upgrade: `npm install -g @informalsystems/quint` (must be ≥ 0.32.0 for the liveness module).
- Both `quint verify` backends require a JDK for Apalache (17+; this repo pins 21 LTS via `crates/ringmpsc/tla/.java-version`).
- The `quint-connect` v0.1.1 MBT driver required **zero code changes** — the upgrade is again purely at the CLI layer.

## Remaining Work

| Item | Status | Notes |
|------|--------|-------|
| CI workflow (`.github/workflows/quint.yml`) | Planned | `quint verify` (safety + liveness, both backends) + MBT in GitHub Actions |
| MPSC channel spec (`RingMPSC.qnt`) | Planned | Multi-producer model using Quint's nondeterminism |
| `q::debug` diagnostics in `.qnt` spec | Planned | Per-step tracing for richer MBT debugging |
| Loom trace export → Quint verification | Planned | Cross-validate Loom interleavings against the model |

## References

- [Quint 0.32.0 Release Notes](https://github.com/informalsystems/quint/releases/tag/v0.32.0)
- [`leadsTo` PR #1932](https://github.com/informalsystems/quint/pull/1932)
- [RingSPSC.qnt](../crates/ringmpsc/tla/RingSPSC.qnt) — safety spec
- [RingSPSCLiveness.qnt](../crates/ringmpsc/tla/RingSPSCLiveness.qnt) — liveness spec
- [QUINT_0_31_UPGRADE.md](QUINT_0_31_UPGRADE.md) — previous upgrade (backend + TLC integration)
