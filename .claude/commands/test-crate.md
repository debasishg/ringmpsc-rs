Run all tests for a single crate, including all applicable feature flag combinations. The crate package name is provided as `$ARGUMENTS`.

Always use `--release` — debug builds break lock-free correctness.

**Feature matrix per crate:**

- `ringmpsc-rs`: run default, `stack-ring`, `quint-mbt`, `allocator-api`, and on Linux `numa`
- `ringmpsc-stream`: run default only
- `ringwal`: run default only
- `ringwal-sim`: run default only
- `ringwal-store`: run default only
- `span_collector`: run default only

**Example for `ringmpsc-rs`:**
```
cargo test -p ringmpsc-rs --release
cargo test -p ringmpsc-rs --features stack-ring --release
cargo test -p ringmpsc-rs --features quint-mbt --release
cargo test -p ringmpsc-rs --features allocator-api --release
```

Print a per-run status table after all runs complete. Show full output only for failures.

If `$ARGUMENTS` is empty, ask the user which crate to test.
