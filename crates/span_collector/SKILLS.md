# Skills — span_collector

Skills relevant to the OpenTelemetry span collector crate (`span_collector`).
See the workspace-level [`SKILLS.md`](../../SKILLS.md) for the full hierarchy.

## Testing

| Skill | Command | What it covers |
|-------|---------|---------------|
| Integration tests | `/test-crate span_collector` | End-to-end resilience: batch export, retry logic, circuit breaker tripping and recovery, rate limiter behaviour |

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
| `spec.md` | Collection invariants (ordering, batching, resilience) |
| `src/invariants.rs` | `debug_assert!` macros for collector invariants |
| `src/exporter.rs` | Async trait object pattern: native trait + boxed companion + blanket impl |
| `src/resilient_exporter.rs` | Retry + circuit breaker wrapper |
| `src/rate_limiter.rs` | Token bucket rate limiter |
| `src/async_bridge.rs` | Object-safe async trait boxing utilities |

## Notes

- `impl Future` in traits is not object-safe — this crate is the canonical example of the boxed-trait + blanket impl pattern used across the workspace
- Circuit breaker state is per-exporter instance; reset requires a configurable cooldown period
- Rate limiter uses token bucket — burst capacity is bounded by initial token count
