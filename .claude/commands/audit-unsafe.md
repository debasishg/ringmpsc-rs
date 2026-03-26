Audit every `unsafe` block across all crate source files for compliance with the project's mandatory safety documentation rules.

Search all `.rs` files under `crates/*/src/` for `unsafe` blocks. For each one found, check:

1. **Safety comment present** — the line immediately before the `unsafe` keyword (or within 3 lines) must contain `// Safety:`
2. **INV-* citation** — the Safety comment must reference at least one `INV-*` identifier (pattern: `INV-[A-Z]+-[0-9]+`)

Report violations in this format:

```
VIOLATION: crates/ringmpsc/src/ring.rs:147
  unsafe block missing // Safety: comment

VIOLATION: crates/ringwal/src/wal.rs:89
  // Safety: comment present but no INV-* identifier cited
```

After scanning, print a summary:

| Category | Count |
|----------|-------|
| Total unsafe blocks found | N |
| Missing Safety comment | N |
| Missing INV-* citation | N |
| Fully compliant | N |

A fully compliant codebase should show 0 violations. Any violation must be fixed before merge.
