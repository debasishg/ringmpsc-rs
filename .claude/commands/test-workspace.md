Run the full test matrix across all workspace crates and feature flag combinations. Always use `--release` — debug builds break lock-free correctness.

Execute these commands in order:

```
cargo test --workspace --release
cargo test -p ringmpsc-rs --features stack-ring --release
cargo test -p ringmpsc-rs --features quint-mbt --release
cargo test -p ringmpsc-rs --features allocator-api --release
cargo test -p ringmpsc-stream --release
cargo test -p ringwal --release
cargo test -p ringwal-sim --release
cargo test -p span_collector --release
```

For the `numa` feature, check if running on Linux before attempting:
```
cargo test -p ringmpsc-rs --features numa --release   # Linux only
```

After all runs, print a summary table:

| Command | Status |
|---------|--------|
| workspace (default) | PASS / FAIL |
| ringmpsc-rs stack-ring | PASS / FAIL |
| ringmpsc-rs quint-mbt | PASS / FAIL |
| ringmpsc-rs allocator-api | PASS / FAIL |
| ringmpsc-stream | PASS / FAIL |
| ringwal | PASS / FAIL |
| ringwal-sim | PASS / FAIL |
| span_collector | PASS / FAIL |

Show full output only for failing runs.
