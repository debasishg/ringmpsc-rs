Run the full pre-merge verification suite for this workspace. Execute the following three checks in order, stopping and reporting failure immediately if any step fails:

1. Run `/verify-concurrency` — loom exhaustive concurrency testing + miri undefined behaviour detection for `ringmpsc-rs`.
2. Run `/verify-spec` — TLA+/Quint model checking + `quint-mbt` ITF trace replay for `ringmpsc-rs`.
3. Run `/verify-invariants` — static cross-check of `INV-*` IDs between every crate's `spec.md` and `src/invariants.rs`.

After all three pass, print a single summary table:

| Check | Status |
|-------|--------|
| verify-concurrency | PASS / FAIL |
| verify-spec | PASS / FAIL |
| verify-invariants | PASS / FAIL |

If any check fails, show its full output and do not run subsequent checks.
