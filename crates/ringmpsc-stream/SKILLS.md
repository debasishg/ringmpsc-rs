# Skills — ringmpsc-stream

Skills relevant to the async Stream/Sink adapter crate (`ringmpsc-stream`).
See the workspace-level [`SKILLS.md`](../../SKILLS.md) for the full hierarchy.

## Testing

| Skill | Command | What it covers |
|-------|---------|---------------|
| Integration tests | `/test-crate ringmpsc-stream` | Async Stream/Sink end-to-end: backpressure, graceful shutdown, waker propagation |

## Verification

| Skill | Command | What it checks |
|-------|---------|---------------|
| Invariant sync | `/verify-invariants` | Cross-checks every `INV-*` ID in `spec.md` against `debug_assert!` macros in `src/invariants.rs` |
| Spec drift | `/spec-sync` | Matched / spec-only / code-only INV-* IDs for this crate |

## Audit

| Skill | Command | What it checks |
|-------|---------|---------------|
| Unsafe block audit | `/audit-unsafe` | Every `unsafe` block has `// Safety:` + `INV-*` citation |

## Key files

| File | Purpose |
|------|---------|
| `spec.md` | Async streaming semantics and invariants |
| `src/invariants.rs` | `debug_assert!` macros for stream invariants |
| `src/receiver.rs` | Consumer-side `Stream` implementation |
| `src/sender.rs` | Producer-side `Sink` implementation |
| `src/shutdown.rs` | Graceful shutdown protocol |
| `DESIGN.md` | Hybrid polling design rationale |

## Notes

- This crate wraps `ringmpsc` — correctness depends on `ringmpsc` invariants holding
- Backpressure: `Sink::poll_ready` returns `Pending` when the underlying ring is full; callers must not spin
- The async trait object pattern (boxed companion + blanket impl) is used in `src/` — see `CLAUDE.md` for the pattern
