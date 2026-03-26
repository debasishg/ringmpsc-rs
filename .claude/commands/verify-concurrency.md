Run concurrency verification for the `ringmpsc-rs` crate. This covers two complementary tools:

**Step 1 — Loom (exhaustive state-space exploration)**
```
cargo test -p ringmpsc-rs --features loom --test loom_tests --release
```
Loom replaces std atomics with a model that explores every valid thread interleaving. This is slow by design — do not interrupt it. If it finds a violation it will print the minimal failing schedule.

**Step 2 — Miri (undefined behaviour detection)**
```
cargo +nightly miri test -p ringmpsc-rs --test miri_tests
```
Miri interprets Rust MIR to detect use-after-free, out-of-bounds reads, invalid `MaybeUninit` access, and data races. Requires nightly toolchain.

Report each step's exit status. If either fails, show the full error output and identify which invariant (`INV-*`) the failure relates to if determinable.

Note: always `--release` — debug builds break lock-free algorithms by changing code layout and disabling optimisations the safety proofs rely on.
