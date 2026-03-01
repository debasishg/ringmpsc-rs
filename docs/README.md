# Documentation Index

This folder contains design documents, guides, and technical references for the `ringmpsc-rs` workspace.

## Document Map

| Document | Audience | Purpose |
|----------|----------|---------|
| [UB_FIX_MAKE_RESERVATION.md](UB_FIX_MAKE_RESERVATION.md) | Rust developers | Bug postmortem for the UB in `make_reservation` — debug vs release divergence explained |
| [PERSISTENT_CONTEXT_GUIDE.md](PERSISTENT_CONTEXT_GUIDE.md) | AI agent users, maintainers | How to structure project knowledge so AI coding agents stay accurate |
| [QUINT_TRANSLATION.md](QUINT_TRANSLATION.md) | Spec authors | Manual TLA+ → Quint translation reference |
| [FORMAL_VERIFICATION_WORKFLOW.md](FORMAL_VERIFICATION_WORKFLOW.md) | All contributors | Quick end-to-end overview of the TLA+ → Quint → Rust verification pipeline |
| [self-verified-pipeline.md](self-verified-pipeline.md) | Deep dives | Detailed visual walkthrough of the full invariant validation pipeline |
| [FORMAL_METHODS_AGENTIC_DEVELOPMENT.md](FORMAL_METHODS_AGENTIC_DEVELOPMENT.md) | Architecture | Conceptual rationale for integrating formal methods into AI-assisted development |
| [model-based-testing-in-agentic-development.md](model-based-testing-in-agentic-development.md) | MBT practitioners | Evolution from hand-crafted MBT to `quint-connect` automated trace generation |
| [tokio-fairness.md](tokio-fairness.md) | Async layer maintainers | Tokio scheduling fairness trade-offs contextualized to this workspace |
| [QUINT_0_31_UPGRADE.md](QUINT_0_31_UPGRADE.md) | Maintainers | Impact of the Quint 0.31.0 upgrade on the verification infrastructure |

## Suggested Reading Order

**New contributors** — start here:
1. `FORMAL_VERIFICATION_WORKFLOW.md` — understand what layers exist and why
2. `QUINT_TRANSLATION.md` — learn the TLA+ → Quint mapping
3. `model-based-testing-in-agentic-development.md` — understand the current MBT approach

**Going deeper**:
4. `self-verified-pipeline.md` — visual deep-dive into the full pipeline
5. `FORMAL_METHODS_AGENTIC_DEVELOPMENT.md` — architectural rationale

**Specific topics**:
- Toolchain maintenance: `QUINT_0_31_UPGRADE.md`
- AI agent setup: `PERSISTENT_CONTEXT_GUIDE.md`
- Async scheduling: `tokio-fairness.md`
- UB postmortem: `UB_FIX_MAKE_RESERVATION.md`

## Naming Note

The formal spec files are named `RingSPSC` because the spec models the **single-producer
single-consumer** (SPSC) base protocol. The repository name `ringmpsc-rs` reflects the
multi-producer extension built on top. The MPSC spec is tracked as planned work in
`QUINT_0_31_UPGRADE.md`.
