# Documentation Index

> **Last updated**: 2026-03-24

This folder contains design documents, guides, and technical references for the `ringmpsc-rs` workspace.

## Document Map

| Document | Audience | Purpose |
|----------|----------|---------|
| [UB_FIX_MAKE_RESERVATION.md](UB_FIX_MAKE_RESERVATION.md) | Rust developers | Bug postmortem for the UB in `make_reservation` — debug vs release divergence explained |
| [PERSISTENT_CONTEXT_GUIDE.md](PERSISTENT_CONTEXT_GUIDE.md) | AI agent users, maintainers | How to structure project knowledge so AI coding agents stay accurate |
| [QUINT_TRANSLATION.md](QUINT_TRANSLATION.md) | Spec authors | Manual TLA+ → Quint translation reference |
| [FORMAL_VERIFICATION_WORKFLOW.md](FORMAL_VERIFICATION_WORKFLOW.md) | All contributors | Quick end-to-end overview of the TLA+ → Quint → Rust verification pipeline |
| [self-verified-pipeline.md](self-verified-pipeline.md) | All contributors | Detailed visual walkthrough of the full invariant validation pipeline |
| [FORMAL_METHODS_AGENTIC_DEVELOPMENT.md](FORMAL_METHODS_AGENTIC_DEVELOPMENT.md) | Architects, AI agent integrators | Conceptual rationale for integrating formal methods into AI-assisted development |
| [model-based-testing-in-agentic-development.md](model-based-testing-in-agentic-development.md) | MBT practitioners | Evolution from hand-crafted MBT to `quint-connect` automated trace generation |
| [tokio-fairness.md](tokio-fairness.md) | Async layer maintainers | Tokio scheduling fairness trade-offs contextualized to this workspace |
| [CUSTOM_ALLOCATORS.md](CUSTOM_ALLOCATORS.md) | All contributors | Custom allocator subsystem — design, API, testing, formal verification |
| [QUINT_0_31_UPGRADE.md](QUINT_0_31_UPGRADE.md) | Maintainers | Impact of the Quint 0.31.0 upgrade on the verification infrastructure |

## Suggested Reading Order

**New contributors** — start here:
1. `FORMAL_VERIFICATION_WORKFLOW.md` — understand what layers exist and why
2. `QUINT_TRANSLATION.md` — learn the TLA+ → Quint mapping
3. `model-based-testing-in-agentic-development.md` — understand the current MBT approach

> **Note**: If using Quint ≥ 0.31.0, read `QUINT_0_31_UPGRADE.md` after step 1 for context
> on the two-backend verification strategy and Rust evaluator.

**Going deeper**:
4. `self-verified-pipeline.md` — visual deep-dive into the full pipeline
5. `FORMAL_METHODS_AGENTIC_DEVELOPMENT.md` — architectural rationale

**Specific topics**:
- Custom allocators: `CUSTOM_ALLOCATORS.md`
- Toolchain maintenance: `QUINT_0_31_UPGRADE.md`
- AI agent setup: `PERSISTENT_CONTEXT_GUIDE.md`
- Async scheduling: `tokio-fairness.md`
- UB postmortem: `UB_FIX_MAKE_RESERVATION.md`

## Crate-Level Documentation

Several crates maintain their own docs alongside their source. These are **not** duplicated here — see the crate directories directly.

### ringwal

| Document | Purpose |
|----------|---------|
| [architecture.md](../crates/ringwal/docs/architecture.md) | WAL architecture: ring decomposition, group commit, segment rotation |
| [benchmarks.md](../crates/ringwal/docs/benchmarks.md) | Criterion benchmark results (durable, pipelined, scaling) |
| [PIPELINED_FSYNC.md](../crates/ringwal/docs/PIPELINED_FSYNC.md) | Pipelined fsync design and implementation |
| [PIPELINED_IMPROVEMENTS.md](../crates/ringwal/docs/PIPELINED_IMPROVEMENTS.md) | Pipelined mode performance improvements |
| [DIRECT_IO.md](../crates/ringwal/docs/DIRECT_IO.md) | Direct I/O support for WAL writes |
| [dst.md](../crates/ringwal/docs/dst.md) | Deterministic simulation testing overview |
| [dst_arch.md](../crates/ringwal/docs/dst_arch.md) | DST architecture and ringwal-sim design |

### span_collector

| Document | Purpose |
|----------|---------|
| [backpressure-notify-pattern.md](../crates/span_collector/docs/backpressure-notify-pattern.md) | Backpressure and notify pattern for async batching |

### ringmpsc

| Document | Purpose |
|----------|---------|
| [spec.md](../crates/ringmpsc/spec.md) | Ring buffer invariant specifications (INV-*) |
| [PERFORMANCE.md](../crates/ringmpsc/PERFORMANCE.md) | Performance characteristics and tuning |
| [STACK_RING_IMPL.md](../crates/ringmpsc/STACK_RING_IMPL.md) | StackRing\<T, N\> implementation notes |
| [FAQ.md](../crates/ringmpsc/FAQ.md) | Frequently asked questions |
| [tla/README.md](../crates/ringmpsc/tla/README.md) | TLA+ / Quint specification guide |

## Naming Note

The formal spec files are named `RingSPSC` because the spec models the **single-producer
single-consumer** (SPSC) base protocol. The repository name `ringmpsc-rs` reflects the
multi-producer extension built on top. The MPSC spec is tracked as planned work in
`QUINT_0_31_UPGRADE.md`.
