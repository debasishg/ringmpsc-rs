Produce a diff of `INV-*` identifiers between `spec.md` and `src/invariants.rs` to detect drift between the formal specification and the code-level assertions.

If a crate name is provided as `$ARGUMENTS` (e.g. `ringmpsc`, `ringwal`), check only that crate. Otherwise check all crates that have both files: `ringmpsc`, `ringmpsc-stream`, `ringwal`, `ringwal-store`, `span_collector`.

For each crate:
1. Extract all `INV-*` IDs from `spec.md` (pattern `INV-[A-Z]+-[0-9]+`)
2. Extract all `INV-*` IDs from `src/invariants.rs`
3. Classify each ID:
   - **Matched** — present in both (good)
   - **Spec-only** — in `spec.md` but absent from `invariants.rs` (invariant specified but not enforced in code)
   - **Code-only** — in `invariants.rs` but absent from `spec.md` (assertion exists but spec is missing the corresponding invariant)

Output per crate:

```
ringmpsc
  Matched (12):  INV-INIT-01, INV-INIT-02, INV-SEQ-01, ...
  Spec-only (1): INV-WRAP-03  ← add debug_assert! for this
  Code-only (0): —
```

Exit with a non-zero summary if any crate has Spec-only or Code-only entries, so this can be used in CI.
