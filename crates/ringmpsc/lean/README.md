# ringlean: Lean 4 proofs for `window.rs`

Phase 0 of [`RINGLEAN_DESIGN.md`](../../../RINGLEAN_DESIGN.md). It proves, for all inputs, the index and window arithmetic that `Ring` and `StackRing` pass to their `unsafe` code. The proofs are about the production source: Charon and Aeneas translate `crates/ringmpsc/src/window.rs` into Lean, so there is no hand-written model to drift from the code.

| Path | Contents |
|---|---|
| `RingmpscRs/` | **Generated** by Aeneas from `window.rs`. Never edit; rerun `scripts/check.sh`. |
| `RingLean/Window.lean` | Hand-written theorems about the generated definitions. |
| `scripts/check.sh` | Regenerates `RingmpscRs/`, checks the proofs, audits axioms, checks citations. |
| `scripts/Audit.lean` | Axiom audit: every theorem in `RingLean.*` may use only `propext`, `Classical.choice`, `Quot.sound`. |
| `lakefile.toml`, `lake-manifest.json`, `lean-toolchain` | Lean project, pinned to the Aeneas release that generates `RingmpscRs/`. |

## What is proved

| Theorem | Property | Used by |
|---|---|---|
| `slot_index_spec` | `slot_index(seq, cap-1) = seq mod cap < cap` | consume loops |
| `occupancy_spec`, `occupancy_val` | `occupancy` is exact under INV-SEQ-01; truncated to `usize` otherwise | `len`, consume, `Drop` |
| `free_space_spec` | `free_space ≤ cap`; equals `cap - (tail - head)` under INV-SEQ-01 | `reserve` |
| `free_space_stale_conservative` | a stale `cached_head` never reports more space than the real `head` | `reserve` fast path |
| `write_window_spec` | window starts at `tail mod cap`, `idx + len ≤ cap`, `len ≤ n`, `len = min(n, cap - idx)`, non-empty if `n ≥ 1` | `make_reservation` |
| `read_window_spec` | `None` iff `tail ≤ head` (#17); otherwise `idx + len ≤ cap`, `len ≤ tail - head`, non-empty under INV-SEQ-01 | `readable`, `peek` |
| `consume_count_spec` | `consume_count = min(avail, max)` | `consume_up_to*` |
| `windows_disjoint`, `reservation_disjoint` | a reservation `free_space` allows, laid out by `write_window`, maps to no buffer slot in the consumer's `[head, tail)` | `make_reservation` |

Every `_spec` theorem also proves the function **cannot panic**: Aeneas models overflow as failure, and the specs state the result is `ok`.

**`usize` width.** Aeneas leaves the width of `usize` abstract (32 or 64 bits), so `u64 as usize` is a truncating cast in the model, and every theorem holds on both widths. Two results depend on the premise `tail - head ≤ cap` (INV-SEQ-01) for exactly this reason: on a 32-bit target, without it, a distance of `2^32` would truncate to 0.

**No `u64` wrap-around.** The theorems use real `u64` arithmetic but do not cover sequence numbers wrapping past `u64::MAX`. `read_window` compares `tail <= head` without wrapping, so after a wrap it would report the ring as empty, and `reservation_disjoint` assumes `head ≤ tail`. INV-SEQ-03 rules wrap out (about 584 years at 10⁹ messages per second).

**Assumed, not proved here.** Premises named after `spec.md` invariants (INV-SEQ-01, INV-SEQ-03, INV-MEM-02) are maintained by the ring protocol and covered by Verus, Quint and MBT. Memory orderings are covered by loom and Miri.

## Citations

`// Safety:` comments in `ring.rs` and `stack_ring.rs` cite theorems as `[lean: write_window_spec, ...]`, next to the `INV-*` IDs they rely on. Keep each `[lean: ...]` list on one line. `check.sh` fails if a cited name is not an audited theorem, or if `src/` has no citations at all.

## Running

```bash
crates/ringmpsc/lean/scripts/check.sh                     # regenerate + prove + audit
crates/ringmpsc/lean/scripts/check.sh --check-generated   # also fail if RingmpscRs/ is stale (CI)
```

`/verify` runs it (step 4). `cargo build` and `cargo test` do not; see the design doc, "When proofs run". There is no CI job yet.

### One-time setup

```bash
# Lean version manager
brew install elan-init              # or: curl https://elan.lean-lang.org/elan-init.sh -sSf | sh

# Aeneas release (bundles aeneas, charon, charon-driver); must match `rev` in lakefile.toml
mkdir -p ~/tools/aeneas && cd ~/tools/aeneas
gh release download nightly-2026.11.24-fb44cb4 -R AeneasVerif/aeneas -p 'aeneas-macos-aarch64.tar.gz'
tar xzf aeneas-macos-aarch64.tar.gz

# Charon's Rust toolchain (see ~/tools/aeneas/rust-toolchain)
rustup toolchain install nightly-2026-09-17 -c rustc-dev -c llvm-tools -c rust-src -c miri

# Lean dependencies: Aeneas library and the Mathlib cache (several GB, first time only)
cd crates/ringmpsc/lean && lake update
```

Set `AENEAS_DIR` if the release is unpacked somewhere other than `~/tools/aeneas`. Charon builds into `target/charon` so its nightly artifacts don't mix with normal builds.

### Upgrading Aeneas

Change the release tag above, `rev` in `lakefile.toml`, and `lean-toolchain` together (`lean-toolchain` must match `backends/lean/lean-toolchain` in that Aeneas commit), then `lake update` and rerun `check.sh`.

## Adding a function

1. Write it in `window.rs`: safe, pure, no generics, `if`/`else` rather than `min`/`saturating_sub`.
2. Run `check.sh` to regenerate `RingmpscRs/Funs.lean`.
3. Add a `_spec` theorem to `RingLean/Window.lean`, stating the postcondition as `f args ⦃ r => ... ⦄`. A new file in `RingLean/` also needs an `import` in `scripts/Audit.lean`.
4. Cite it from the `// Safety:` comment that relies on it.
5. Mutation-check it: break the function in `window.rs` and confirm `check.sh` fails.
