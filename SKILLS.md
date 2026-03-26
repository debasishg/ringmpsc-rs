# Skills — ringmpsc-rs Workspace

Slash commands available across this workspace. Invoke with `/command-name` in Claude Code.
Per-crate `SKILLS.md` files document which skills are relevant to each crate specifically.

## Tier 1 — Orchestrators

| Skill | Command | Scope |
|-------|---------|-------|
| Full pre-merge verification | `/verify` | Workspace |

## Tier 2 — Verification (mandatory before merge)

| Skill | Command | Scope |
|-------|---------|-------|
| Loom + Miri concurrency testing | `/verify-concurrency` | `ringmpsc` only |
| TLA+/Quint model checking + ITF trace replay | `/verify-spec` | `ringmpsc` only |
| spec.md ↔ invariants.rs INV-* cross-check | `/verify-invariants` | All crates |

## Tier 3 — Testing

| Skill | Command | Scope |
|-------|---------|-------|
| Full test matrix, all crates + feature flags | `/test-workspace` | Workspace |
| All tests for a single crate | `/test-crate <pkg-name>` | Single crate |

## Tier 4 — Audit (static analysis, no test execution)

| Skill | Command | Scope |
|-------|---------|-------|
| Unsafe block Safety comment + INV-* audit | `/audit-unsafe` | All crates |
| Atomic ordering correctness audit | `/audit-ordering` | All crates |

## Tier 5 — Crate Lifecycle

| Skill | Command | Scope |
|-------|---------|-------|
| Scaffold new crate (Cargo.toml, spec.md, invariants.rs) | `/new-crate <name>` | Workspace |
| Diff INV-* IDs between spec.md and invariants.rs | `/spec-sync` | All crates |

## Tier 6 — Performance

| Skill | Command | Scope |
|-------|---------|-------|
| Criterion benchmarks with results summary | `/bench [stack-ring\|ringwal\|allocator]` | `ringmpsc`, `ringwal` |

## Tier 7 — Documentation

| Skill | Command | Scope |
|-------|---------|-------|
| Review docs/ and crates/ documentation quality | `/docreview` | Workspace |

---

## Composition

`/verify` composes Tier 2 skills:
```
/verify
├── /verify-concurrency
├── /verify-spec
└── /verify-invariants
```

`/new-crate` generates a `SKILLS.md` skeleton in the new crate directory automatically.

## Adding New Skills

1. Create `.claude/commands/<name>.md` with the executable prompt
2. Add a row to the relevant tier table above
3. Add a row to the per-crate `SKILLS.md` for any crate the skill is scoped to
