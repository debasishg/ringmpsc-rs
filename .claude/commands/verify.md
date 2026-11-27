Run the full pre-merge verification suite for this workspace. Execute the following four checks in order, stopping and reporting failure immediately if any step fails:

1. Run `/verify-concurrency` — loom exhaustive concurrency testing + miri undefined behaviour detection for `ringmpsc-rs`.
2. Run `/verify-spec` — TLA+/Quint model checking + `quint-mbt` ITF trace replay for `ringmpsc-rs`.
3. Run `/verify-invariants` — static cross-check of `INV-*` IDs between every crate's `spec.md` and `src/invariants.rs`.
4. Run `crates/ringmpsc/lean/scripts/check.sh --check-generated` — regenerate the Lean model of `crates/ringmpsc/src/window.rs` (Charon + Aeneas), check every theorem, audit axioms, and check the `[lean: ...]` citations in `// Safety:` comments. Fails if `RingmpscRs/` in the working tree was stale, i.e. `window.rs` changed without rerunning `check.sh`. Regenerated files that aren't committed yet pass. Setup: `crates/ringmpsc/lean/README.md`.

After all four pass, print a single summary table:

| Check | Status |
|-------|--------|
| verify-concurrency | PASS / FAIL |
| verify-spec | PASS / FAIL |
| verify-invariants | PASS / FAIL |
| lean-proofs | PASS / FAIL |

If any check fails, show its full output and do not run subsequent checks.
