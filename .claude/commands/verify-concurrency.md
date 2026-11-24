Run concurrency verification for the `ringmpsc-rs` crate. This covers two complementary tools, in three steps:

**Step 1 — Loom (exhaustive state-space exploration)**
```
cargo test -p ringmpsc-rs --features loom --test loom_tests --release
```
Loom replaces std atomics with a model that explores every valid thread interleaving. This is slow by design — do not interrupt it. If it finds a violation it will print the minimal failing schedule.

**Step 2 — Miri (undefined behaviour detection)**
```
cargo +nightly miri test -p ringmpsc-rs --features stack-ring --test miri_tests
```
Miri interprets Rust MIR to detect use-after-free, out-of-bounds reads, invalid `MaybeUninit` access, and data races. Requires nightly toolchain.

**Step 3 — Miri, threaded StackRing / StackChannel**
```
MIRIFLAGS="-Zmiri-preemption-rate=0.1" \
  cargo +nightly miri test -p ringmpsc-rs --features stack-ring --test miri_stack_overlap_test
```
Runs a `StackRing` and a `StackChannel` producer concurrently with their consumer (a few seconds under Miri). The preemption rate makes Miri switch threads often enough for the two to overlap.

Run all three steps even if one fails, and report each step's exit status separately. If any fails, show its full error output and identify which invariant (`INV-*`) the failure relates to if determinable.

Note: loom runs with `--release` — debug builds break lock-free algorithms by changing code layout and disabling optimisations the safety proofs rely on. Miri takes no `--release`: it interprets MIR rather than running compiled code.
