# Skills — ringmpsc

Skills relevant to the core lock-free SPSC/MPSC channel crate (`ringmpsc-rs`).
See the workspace-level [`SKILLS.md`](../../SKILLS.md) for the full hierarchy.

## Verification (run before every merge touching this crate)

| Skill | Command | What it checks |
|-------|---------|---------------|
| Concurrency verification | `/verify-concurrency` | Loom exhaustive state-space exploration of all atomic interleavings + Miri undefined behaviour detection for `unsafe`/`MaybeUninit` code |
| Formal spec verification | `/verify-spec` | TLA+ model checker (`RingSPSC.tla`) + Quint model-based tests replaying ITF traces against the Rust implementation |
| Invariant sync | `/verify-invariants` | Cross-checks every `INV-*` ID in `spec.md` against `debug_assert!` macros in `src/invariants.rs` |

Run all three at once with `/verify`.

## Testing

| Skill | Command | Feature flags covered |
|-------|---------|----------------------|
| Full crate test matrix | `/test-crate ringmpsc-rs` | default, `stack-ring`, `quint-mbt`, `allocator-api`, `numa` (Linux) |

## Audit

| Skill | Command | What it checks |
|-------|---------|---------------|
| Unsafe block audit | `/audit-unsafe` | Every `unsafe` block has `// Safety:` + `INV-*` citation |
| Memory ordering audit | `/audit-ordering` | Acquire/Release/Relaxed used per CLAUDE.md table; no SeqCst |

## Performance

| Skill | Command | Benchmarks |
|-------|---------|-----------|
| Heap benchmarks | `/bench` | `throughput.rs` — SPSC and MPSC msg/s across producer counts |
| Stack vs heap | `/bench stack-ring` | `stack_vs_heap.rs` — compile-time capacity vs heap allocation |
| Allocator overhead | `/bench allocator` | `allocator.rs` — custom allocator vs default |

## Spec Sync

| Skill | Command | Output |
|-------|---------|--------|
| INV-* drift detection | `/spec-sync` | Matched / spec-only / code-only IDs for this crate |

## Key files

| File | Purpose |
|------|---------|
| `spec.md` | Canonical invariant spec — source of all `INV-*` IDs |
| `src/invariants.rs` | `debug_assert!` macros enforcing spec invariants at runtime |
| `tla/RingSPSC.tla` | TLA+ formal model |
| `tla/RingSPSC.qnt` | Quint model (ITF trace source) |
| `FAQ.md` | Design rationale |
| `PERFORMANCE.md` | Benchmark analysis |
| `ring-optimization.md` | Low-level optimization analysis (cache, NUMA, atomics, allocator) |
| `numa-aware-allocation.md` | NUMA allocator design and caveats |
