# RingMPSC-RS — Claude Project Context

Lock-free Rust workspace built on **ring decomposition** (each producer owns a dedicated SPSC ring, eliminating producer-producer contention). Targets 6–7 billion messages/second.

## Workspace Crates

| Crate | Pkg name (`-p`) | Purpose | Spec |
|-------|-----------------|---------|------|
| `crates/ringmpsc` | `ringmpsc-rs` | Core SPSC/MPSC channel, heap + stack variants | `spec.md` |
| `crates/ringmpsc-stream` | `ringmpsc-stream` | Async `Stream`/`Sink` with backpressure | `spec.md` |
| `crates/ringwal` | `ringwal` | WAL with per-writer rings, group commit, crash recovery | `spec.md` |
| `crates/ringwal-store` | `ringwal-store` | Storage backend trait + recovery bridge | `spec.md` |
| `crates/ringwal-sim` | `ringwal-sim` | Deterministic simulation testing for ringwal | — |
| `crates/span_collector` | `span_collector` | OpenTelemetry span batching/export | `spec.md` |

`ringmpsc` is the **default workspace member** — `cargo test` without `-p` only runs ringmpsc tests.

**Edition**: 2021 (most crates). **Resolver**: 2.

### Key Source Files (ringmpsc)
`ring.rs` (core SPSC), `channel.rs` (MPSC), `stack_ring.rs` / `stack_channel.rs` (stack variants), `numa.rs`, `allocator.rs`, `invariants.rs`, `reservation.rs`.

## Development Commands

```bash
# Always --release: debug builds break lock-free code
cargo build --release && cargo test --workspace --release

# Crate-specific
cargo test -p ringmpsc-rs --features stack-ring --release
cargo test -p ringmpsc-stream --release
cargo test -p ringwal --release

# Concurrency verification — mandatory before merge when touching atomics/ordering/unsafe
cargo test -p ringmpsc-rs --features loom --test loom_tests --release
cargo +nightly miri test -p ringmpsc-rs --test miri_tests

# Model-based testing (Quint spec → ITF traces → Rust assertions)
cargo test -p ringmpsc-rs --features quint-mbt --release

# Benchmarks
cargo bench -p ringmpsc-rs
cargo bench -p ringmpsc-rs --features stack-ring --bench stack_vs_heap
cargo bench -p ringwal

# TLA+ model checking
cd crates/ringmpsc/tla && tlc RingSPSC.tla -config RingSPSC.cfg -workers auto
```

## Critical Rules

### `--release` is Required for Correctness
Lock-free algorithms rely on optimized codegen. Never run correctness or concurrency tests in debug mode.

### Unsafe — Mandatory `// Safety:` Comment
Every `unsafe` block must cite which invariant (`INV-*`) makes it sound:
```rust
// Safety: idx bounded by mask, slot initialized (head ≤ idx < tail per INV-INIT-01)
unsafe { buffer[idx].assume_init_read() }
```

### Invariants Module
Each crate has `src/invariants.rs` with `debug_assert!` macros keyed to `INV-*` IDs from `spec.md`. When adding invariant checks, add the macro there and reference the spec ID.

### Memory Ordering (Minimum Sufficient)
| Pattern | Ordering |
|---------|----------|
| Metrics counters | `Relaxed` |
| Single-writer cache (`UnsafeCell`) | n/a |
| Publish data to consumer | `Release` |
| Read published data | `Acquire` |

### Reservation API — Critical Footgun
`reserve(n)` **may return fewer slots than `n`** due to ring wrap-around. Always loop:
```rust
while remaining > 0 {
    if let Some(mut r) = ring.reserve(remaining) {
        remaining -= r.len(); // MAY BE < remaining
        r.as_mut_slice()[0] = MaybeUninit::new(item);
        r.commit();
    }
}
```

### `MaybeUninit` Writes
Use `slot.write(value)` — never `*slot = value` (drops an uninitialized value → UB).

### Cache Line Alignment
Hot atomics need `#[repr(align(128))]` — Intel prefetches 2 × 64-byte lines.

### Backpressure
Ring full → `reserve()` returns `None`. Retry with backoff; never spin.

### Async Trait Objects (`span_collector`)
`impl Future` in traits is not object-safe. Pattern: native trait + boxed companion trait + blanket impl. See `span_collector/src/exporter.rs`.

## Error Handling

| Context | Rule |
|---------|------|
| Library | `Result`/`Option` — never `.unwrap()` |
| Tests | `.expect("reason")` |
| Binary entry | `.expect("fatal: reason")` |

## Clippy

Pedantic baseline. Workspace `Cargo.toml` allows: `cast_possible_truncation`, `cast_possible_wrap`, `cast_sign_loss`, `too_many_lines`, `module_name_repetitions`, and several formatting lints. Check root `Cargo.toml` `[workspace.lints.clippy]` before adding new allows.

## Feature Flags

| Flag | Crate | Effect |
|------|-------|--------|
| `stack-ring` | ringmpsc-rs | `StackRing`/`StackChannel` — 2–3× faster, compile-time capacity |
| `loom` | ringmpsc-rs | Replace std atomics with loom for exhaustive concurrency testing |
| `quint-mbt` | ringmpsc-rs | Model-based tests driven by Quint spec + ITF traces |
| `allocator-api` | ringmpsc-rs | Nightly custom allocator API |
| `numa` | ringmpsc-rs | NUMA-aware allocation via `libc` |

## Adding New Crates

1. `crates/<name>/Cargo.toml` + `src/lib.rs`; add to `[workspace.members]`
2. Create `spec.md` with `INV-*` invariants
3. Create `src/invariants.rs` with `debug_assert!` macros per spec
4. Use `workspace = true` for all shared deps

## Skills (Slash Commands)

See `SKILLS.md` at the workspace root for the full hierarchy. Quick reference:

| Command | Purpose |
|---------|---------|
| `/verify` | Full pre-merge suite (orchestrates the three below) |
| `/verify-concurrency` | Loom + Miri for `ringmpsc` |
| `/verify-spec` | TLA+/Quint model check + ITF trace replay |
| `/verify-invariants` | spec.md ↔ invariants.rs INV-* cross-check (all crates) |
| `/test-workspace` | Full test matrix, all crates + feature flags |
| `/test-crate <pkg>` | Single crate, all feature combos |
| `/audit-unsafe` | Unsafe blocks missing `// Safety:` or INV-* citation |
| `/audit-ordering` | Atomic ordering correctness (no SeqCst, correct Acquire/Release) |
| `/new-crate <name>` | Scaffold Cargo.toml, spec.md, invariants.rs, SKILLS.md |
| `/spec-sync` | Diff INV-* IDs between spec.md and invariants.rs |
| `/bench [target]` | Criterion benchmarks with results summary |
| `/docreview` | Documentation quality review |

Per-crate `SKILLS.md` files: `crates/ringmpsc/SKILLS.md`, `crates/ringwal/SKILLS.md`, `crates/ringmpsc-stream/SKILLS.md`, `crates/span_collector/SKILLS.md`.

## Key Documentation

| Path | Content |
|------|---------|
| `crates/ringmpsc/spec.md` | Canonical invariant spec — source of all `INV-*` IDs |
| `crates/ringmpsc/FAQ.md` | Design rationale |
| `crates/ringmpsc/PERFORMANCE.md` | Benchmark analysis |
| `crates/ringmpsc/tla/` | TLA+ and Quint formal specs |
| `docs/CUSTOM_ALLOCATORS.md` | Allocator integration guide |
| `docs/FORMAL_VERIFICATION_WORKFLOW.md` | TLA+/Quint workflow |
| `docs/model-based-testing-in-agentic-development.md` | MBT + ITF trace methodology |
| `docs/self-verified-pipeline.md` | End-to-end verification pipeline |
