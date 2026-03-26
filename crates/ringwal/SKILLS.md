# Skills — ringwal

Skills relevant to the Write-Ahead Log crate (`ringwal`).
See the workspace-level [`SKILLS.md`](../../SKILLS.md) for the full hierarchy.

## Testing

| Skill | Command | What it covers |
|-------|---------|---------------|
| Integration tests | `/test-crate ringwal` | Full WAL lifecycle: write, group commit, segment rotation, crash recovery, CRC32 verification |
| Simulation tests | `/test-crate ringwal-sim` | Deterministic simulation via `ringwal-sim`: fault injection (crash, I/O errors), oracle-based recovery validation |

## Verification

| Skill | Command | What it checks |
|-------|---------|---------------|
| Invariant sync | `/verify-invariants` | Cross-checks every `INV-*` ID in `spec.md` against `debug_assert!` macros in `src/invariants.rs` |
| Spec drift | `/spec-sync` | Matched / spec-only / code-only INV-* IDs for this crate |

## Performance

| Skill | Command | Benchmarks |
|-------|---------|-----------|
| WAL throughput | `/bench ringwal` | `wal_throughput.rs` — write/fsync throughput across sync modes (Sync, Pipelined, Background, DataOnly) |

## Audit

| Skill | Command | What it checks |
|-------|---------|---------------|
| Unsafe block audit | `/audit-unsafe` | Every `unsafe` block has `// Safety:` + `INV-*` citation |
| Memory ordering audit | `/audit-ordering` | Acquire/Release/Relaxed correctness in WAL ring integration |

## Key files

| File | Purpose |
|------|---------|
| `spec.md` | Canonical WAL invariant spec |
| `src/invariants.rs` | `debug_assert!` macros for WAL invariants |
| `src/wal.rs` | Core WAL engine |
| `src/recovery.rs` | Crash recovery logic |
| `src/io/real.rs` | Real I/O including macOS `F_NOCACHE` direct I/O |
| `README.md` | Architecture overview, sync mode comparison |

## Notes

- Simulation tests live in the separate `ringwal-sim` crate — use `/test-crate ringwal-sim` to run them
- Direct I/O (`F_NOCACHE`) is macOS-specific; `O_DIRECT` would be needed for Linux
- Pipelined fsync mode provides the best throughput-durability trade-off for most workloads
