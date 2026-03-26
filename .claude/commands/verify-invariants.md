Statically cross-check that every `INV-*` ID defined in each crate's `spec.md` has a corresponding `debug_assert!` macro in `src/invariants.rs`, and vice versa. No compilation required.

If a crate name is provided as `$ARGUMENTS` (e.g. `ringmpsc`, `ringwal`), check only that crate. Otherwise check all crates that have both a `spec.md` and a `src/invariants.rs`: ringmpsc, ringmpsc-stream, ringwal, ringwal-store, span_collector.

For each crate in scope:

1. Extract all `INV-*` identifiers from `spec.md` (pattern: `INV-[A-Z]+-[0-9]+`)
2. Extract all `INV-*` identifiers referenced inside `debug_assert!` macros in `src/invariants.rs`
3. Compute three sets:
   - **Matched** — present in both
   - **Spec-only** — in `spec.md` but missing from `invariants.rs` (invariant not yet enforced in code)
   - **Code-only** — in `invariants.rs` but not in `spec.md` (orphaned assertion, spec needs updating)

Print a table per crate:

| Crate | Matched | Spec-only (missing in code) | Code-only (missing in spec) |
|-------|---------|----------------------------|----------------------------|
| ringmpsc | ... | ... | ... |
| ...

Flag any Spec-only or Code-only entries as warnings — they indicate the spec and implementation have drifted.
