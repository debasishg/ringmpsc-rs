# ringlean: Lean 4 proofs over the production ring

**Status:** Phase 0 implemented on the `ringlean` branch (section 4, "Phase 0 status"). Phase 1 is a proposal.

This document proposes adding Lean 4 proofs to `ringmpsc`. The idea comes from cuda-oxide's Lean verification of GPU kernels, described in [Provably safe GPU kernels](https://nihalpasham.github.io/posts/provably-safe-gpu-kernels/). It builds on the Verus-based verified ring on the main branch (`verus/ringmpsc-verified`) and targets the gap that work leaves open: **Verus proves a reimplementation of the ring, not the production `crates/ringmpsc` code**. Differential tests tie the two rings together, but the proofs don't carry over to production.

**Summary:**

- cuda-oxide's technique carries over to this crate, but only for **index arithmetic and window bounds inside one thread**. Memory orderings, the cross-thread protocol and liveness stay with loom, Miri, Quint and Verus.
- The proposed work has two phases:
  - **Phase 0:** move the arithmetic into pure functions and prove them with Charon and Aeneas. No custom compiler work is needed.
  - **Phase 1:** a custom rustc driver that exports MIR into Lean, with proc-macro annotations. This lets proofs cover the `unsafe fn`s directly. Build it only if Phase 0 shows clear value.
- We can't remove proven checks from the compiled binary the way cuda-oxide does. The payoff is proof about the code that actually ships, plus `// Safety:` comments that cite theorems a machine has checked.

---

## 1. What cuda-oxide does

cuda-oxide compiles Rust GPU kernels to PTX through its own pliron-based pipeline:

```
Rust ─► mir-importer ─► mem2reg ─► [Lean step] ─► mir-lower ─► LLVM ─► ptxas ─► PTX
```

The Lean step runs on that pipeline's MIR after `mem2reg`, when stack variables have become SSA registers and loops are not yet unrolled. At that point every slice access still has its explicit bounds check in front of it.

1. **Annotations.** `#[verify(no_oob_idx(a, b, c), disjoint_writes(c), ...)]` lists the properties to prove. `#[launch_contract(block = ..., requires = ...)]` lists the assumptions the proofs may use.
2. **Export.** `lean-exporter` translates each instruction and barrier, one to one, into a Lean term in a `MirFunc` / `ControlFlowCfg` model, for example `(.assign 6 .index1D)` or `(.assertCond 0 9)`. Float arithmetic becomes `.opaqueVal`, because a float value never changes *which* memory slot a thread touches. Operations it does not model make the export fail, so nothing is silently skipped.
3. **Proof.** Each clause becomes a theorem, for example `theorem verify_no_oob : MirNoOobGlobal ... := by mir_vc mirFunc`, discharged mostly by Lean's integer solver.
4. **Result.**
   - **Pass:** every theorem is proved with no `sorryAx`. The compiler then deletes the proven `mir.assert` checks and continues to PTX.
   - **Fail:** the build stops and prints a counterexample.
5. **Trusted, not proved:** the Rust-to-SSA step, the exporter itself, the stages after the proofs (`mir-lower`, LLVM, `ptxas`), and the launch checks done on the CPU.

The approach works because GPU kernel memory safety comes down to **one thread's index arithmetic, under a launch contract, with data values treated as opaque**.

---

## 2. How this maps onto ringmpsc

### The same shape: one thread's index arithmetic

Treat the payload `T` as opaque, the way cuda-oxide treats floats. What is left in the ring's unsafe code is a small amount of arithmetic on `u64` sequence numbers and a power-of-two mask. From `make_reservation` in `crates/ringmpsc/src/ring.rs`:

```rust
let idx = (tail as usize) & mask;
let contiguous = n.min(self.capacity() - idx);
let slice = unsafe { std::slice::from_raw_parts_mut(self.slot(idx), contiguous) };
```

Two kinds of operation would be treated as places where a proof is required, the way cuda-oxide treats `mir.assert`:

- **calls:** `from_raw_parts`, `from_raw_parts_mut`, `ptr.add`, `slot(idx)`
- **MIR `Assert` terminators:** bounds checks on safe slice indexing

For `make_reservation`, the proof goals would be:

| Goal | Discharged by |
|---|---|
| `capacity = 2^k ∧ mask = 2^k − 1 → idx < capacity` | `bv_decide`, or `Nat.and_two_pow_sub_one_eq_mod` + `omega` |
| `idx + contiguous ≤ capacity` | `omega` |
| `contiguous ≤ n` | `omega` |
| under `tail − head ≤ capacity` and `n ≤ space`: the reserved window `[tail, tail + n)` doesn't overlap `[head, tail)` | `omega` |

### Where it applies

| Function | Property | INV IDs | Past bug |
|---|---|---|---|
| `Ring::make_reservation`, `StackRing::make_reservation` | the slice is inside the buffer and inside the producer's free window | INV-MEM-02, INV-RES-01 | |
| `Ring::readable_shared`, `StackRing::peek` | the slice is inside `[head, cached_tail)` and inside the buffer | INV-INIT-01 | #17 |
| `Ring::advance_shared` | `head + n ≤ tail` | INV-SEQ-01, INV-SEQ-02 | #14 |
| `Reservation::commit` → `commit_prefix` → `Ring::commit_internal` | the published count is ≤ `filled` ≤ `len` ≤ the space at reserve time | INV-SEQ-01, INV-RES-02 | #32 |
| `StackRing::commit` | the same, as a caller precondition | INV-SEQ-01 | #15 |
| `consume_batch*`, `consume_up_to*` | every index read is in `[head, tail)` | INV-INIT-01 | #17 |

The last column matters. Four of the five memory-safety bugs found during the Verus work (#13, #14, #15, #17, #32) were mistakes in this layer, the arithmetic of a single thread. That is the layer this proposal targets.

### The protocol as a launch contract

Some facts can't be established by one thread looking only at its own code:

- `head ≤ tail ≤ head + capacity` (INV-SEQ-01)
- `head` and `tail` only increase (INV-SEQ-02)
- `cached_head ≤ head` and `cached_tail ≤ tail`: the caches may be stale but never ahead
- there is exactly one producer and one consumer

In this design they are **premises**, the equivalent of cuda-oxide's `#[launch_contract]`. Lean does not prove them; it relies on proofs we already have:

- **Verus:** the protocol, for every capacity, on the verified ring
- **Quint:** the protocol, bounded, checked with TLC
- **Quint trace replay (MBT):** that the production `Ring` follows that protocol step by step
- **loom and Miri:** that the Relaxed/Acquire/Release orderings are strong enough for those facts to hold under the real memory model

This is the same split cuda-oxide makes: the `requires` conditions are checked on the CPU at launch, and Lean proves each thread is safe given them.

### What this does not cover

| Concern | Why Lean-on-MIR can't help | Stays with |
|---|---|---|
| Memory orderings | cuda-oxide reasons about races across barrier phases in the GPU model. It has no model of Acquire/Release. | loom, Miri |
| Protocol invariants, FIFO | These span two threads. Exporting single functions doesn't capture them. | Verus, Quint |
| Liveness | Same reason | `RingSPSCLiveness.qnt` |
| Allocator and NUMA contracts | Calls into `libc` and allocators are outside the model | tests, Miri |

---

## 3. Two ways cuda-oxide doesn't transfer

### We don't own the compiler

cuda-oxide inserts its Lean step into a pipeline it owns. We compile with stock `rustc`. The "custom pipeline" would be a **separate rustc driver** that only reads MIR and exports it; the shipped binary is still built by ordinary `rustc`. As a result:

- **Proven checks can't be deleted from the binary.** What we can get instead:
  - **`unchecked` operations with proof behind them.** A proof can justify replacing defensive `saturating_sub` or `min` with `unchecked_sub` or `get_unchecked`.
  - **Removing release-mode checks that guard internal invariants.** A release-mode check that guards **untrusted input** must stay. For example, `assert_head_not_past_tail!` in `advance_shared` checks an `n` supplied by the caller of the public `advance`. A proof can only remove it if every caller is internal and proved.
  - The `debug_assert!` checks already compile out of release builds, so proving them adds assurance but no speed.
- **A pinned nightly toolchain.** The driver needs one, the same cost the Verus crate already pays. Building on `stable_mir` / `rustc_public` instead of internal rustc APIs reduces how often the driver breaks between nightlies.

### Proc macros annotate; they don't translate

A proc macro sees tokens before name resolution and type checking. It never sees MIR, types or control flow, so it can't generate the Lean model. Its job is the same as cuda-oxide's `#[verify]`: record a contract for the driver to read.

```rust
#[lean_verify(slice_in_bounds, window_disjoint)]
#[lean_assume(INV_SEQ_01, cached_head_le_head)]
fn make_reservation(&self, tail: u64, n: usize) -> Reservation<'_, T, A> { ... }
```

- **Normal builds:** the macros pass the function through unchanged.
- **Driver builds** (under `cfg(ringmpsc_lean)`): they emit a tool attribute (`#![register_tool(ringlean)]`, `#[ringlean::verify(...)]`) that the driver reads from HIR.

---

## 4. Phase 0: pure functions + Charon/Aeneas (recommended start)

**Idea:** Move the arithmetic out of the unsafe functions into small pure, safe functions. Prove those with an existing Rust-to-Lean pipeline. The unsafe code then only calls functions that have been proved.

```rust
// crates/ringmpsc/src/window.rs (new, pure, no unsafe)
/// Returns (idx, len) for a reservation of up to `n` slots at `tail`.
pub(crate) fn write_window(tail: u64, n: usize, cap: usize) -> (usize, usize) {
    let idx = (tail as usize) & (cap - 1);
    (idx, n.min(cap - idx))
}

/// Returns (idx, len) of the readable slice at `head`, or None if empty.
pub(crate) fn read_window(head: u64, cached_tail: u64, cap: usize) -> Option<(usize, usize)> { ... }

/// Free space for the producer.
pub(crate) fn free_space(tail: u64, head: u64, cap: usize) -> usize { ... }
```

1. **Refactor.** `Ring` and `StackRing` call these functions. Behavior is unchanged, and the existing tests, MBT, Miri and benchmarks confirm it.
2. **Translate.** Run **Charon** (a rustc driver that extracts LLBC) and **Aeneas** (LLBC to pure Lean) on `window.rs`. This is the kind of code Aeneas handles well: safe, no aliasing, no pointers.
3. **Prove.** Theorems live in a Lake project at `crates/ringmpsc/lean/`. They are named after spec IDs:

   ```lean
   theorem inv_res_01_write_window_in_bounds
       (tail : U64) (n cap : Usize) (hcap : cap.val.isPowerOfTwo) :
       ∀ idx len, write_window tail n cap = ok (idx, len) →
         idx.val + len.val ≤ cap.val ∧ len.val ≤ n.val := by
     ...  -- omega, bv_decide
   ```

4. **Link the proofs to the unsafe code.** Each `// Safety:` comment cites a theorem:

   ```rust
   // Safety: [idx, idx+len) ⊆ buffer by inv_res_01_write_window_in_bounds;
   // disjoint from the consumer's window given INV-SEQ-01.
   ```

   A check confirms that every cited theorem exists in the Lean project and depends only on Lean's standard axioms. (Implemented in `check.sh`, not `/audit-unsafe`.)

**Cost:** days. The tools already exist, and the pure functions are short.
**Gap left:** proof that the unsafe functions use the window results correctly, for example passing `len` and not `n` to `from_raw_parts_mut`, is still done by review and Miri. Phase 1 closes this gap.

### When proofs run

Phase 0 proofs are **not checked by `cargo build` or `cargo test`**. They run as a separate step, like the Quint model checks:

```
charon (on crates/ringmpsc) → aeneas → lean/Generated/Window.lean → lake build
```

- **Where it runs:** in `/verify` (step 4, `check.sh --check-generated`). CI runs the same command in `.github/workflows/lean.yml` on pushes that touch the proved code, the code citing it, or the Lean project. It is path-filtered, so it is not a required check.
- **Proofs can't go stale when the check runs.** The Lean code is regenerated from the source on every run, and `--check-generated` fails if the committed `RingmpscRs/` differs from what `window.rs` generates, including newly generated files. If a change to `window.rs` breaks a property, that run fails.
- **What a pass means:** every theorem is proved and depends only on `propext`, `Classical.choice` and `Quot.sound`. That rules out `sorry`, user-declared axioms and `native_decide` (`scripts/Audit.lean`).
- **What a failure looks like:** Lean names the goal it couldn't prove, and `omega` prints the inequalities it couldn't satisfy.

Running the proofs inside `cargo build`, for example from `build.rs`, is possible but not recommended. Every build of the crate, including downstream users' builds, would need a nightly toolchain, Charon, Aeneas and Lean, and would be slower. cuda-oxide checks proofs at compile time because it owns the compiler and its users already accept that toolchain. Phase 1 keeps the same separate-step model: `cargo ringlean` is a separate command, not part of the normal build.

### What Phase 0 delivers

- **Proved, for all inputs:** the pure window functions return in-bounds, correctly sized windows. This covers every `tail`, `head` and `n` and every power-of-two capacity, with real `u64` and `usize` arithmetic. It does **not** cover `u64` sequence-number wrap-around: `read_window` compares `tail <= head` without wrapping, as the code before it did, and `reservation_disjoint` assumes no wrap. INV-SEQ-03 rules wrap out: at 10⁹ messages per second, a `u64` takes about 584 years to wrap. The proofs are about the production source, translated from it, not about a separate model or reimplementation.
- **Cited, not proved:** that the `unsafe` code uses the results correctly. `// Safety:` comments cite the theorems, and `check.sh` checks that each cited theorem exists and is proved.
- **Assumed:** the protocol premises (Verus, Quint, MBT) and that the memory orderings are strong enough (loom, Miri).

### Phase 0 status

Implemented. Layout and setup are in `crates/ringmpsc/lean/README.md`.

- **`crates/ringmpsc/src/window.rs`:** six pure functions: `slot_index`, `occupancy`, `free_space`, `write_window`, `read_window`, `consume_count`. Every index and slice length that `Ring` and `StackRing` pass to `unsafe` code now comes from one of them. Behavior is unchanged: release tests, Miri and loom pass.
- **`crates/ringmpsc/lean/`:** Aeneas generates `RingmpscRs/Funs.lean` from `window.rs`. `RingLean/Window.lean` holds 13 theorems, including the specs for all six functions (each also proving the function can't panic), stale-cache conservativeness for `free_space`, and `reservation_disjoint`: a reservation that `free_space` allows never maps to a slot in the consumer's `[head, tail)`. All depend only on Lean's standard axioms.
- **`scripts/check.sh`:** regenerates, proves, audits axioms, and checks the `[lean: ...]` citations in `ring.rs` and `stack_ring.rs`.
- **Mutation check:** five deliberate bugs in `window.rs` were each rejected: an off-by-one in `write_window` and in `free_space`, `write_window` ignoring `n` (the #15/#32 class), `read_window` using `<` instead of `<=` (the #17 class), and a wrong mask in `slot_index`.

What we learned:

- **Charon and Aeneas ship as one release.** The macOS Aeneas release bundles `charon` and `charon-driver`, so nothing is built from source. Charon needs its own nightly (`nightly-2026-09-17`) and its own target directory (`target/charon`). It coexists with the Verus pin and the stable toolchain.
- **`usize` width is abstract in Aeneas,** so `u64 as usize` is a truncating cast in the model. The theorems are proved for both 32-bit and 64-bit `usize` (open question 2). On a 32-bit target, `read_window` could return an empty window if `tail - head` were a multiple of `2^32`. INV-SEQ-01 rules that out, and `read_window_spec` states it as a premise.
- **The first proofs needed Aeneas-specific tactics.** `omega` can't see through `Usize` comparisons in `if` conditions, and `scalar_tac` can loop on cast equations. The workarounds are small and noted in `RingLean/Window.lean`.

---

## 5. Phase 1: custom MIR-to-Lean driver (only if Phase 0 shows value)

The full cuda-oxide design, adapted to rustc:

```
crates/ringmpsc (annotated)
   │  cargo ringlean   (rustc_driver / stable_mir, pinned nightly)
   ▼
MIR as built, before optimization  ── analog of "after mem2reg, before unrolling"
   │  ringlean-exporter: one-to-one transcription, fails on unsupported ops
   ▼
crates/ringmpsc/lean/Generated/*.lean   (RingMir deep embedding + theorems)
   │  lake build; reject any theorem whose axioms include sorryAx
   ▼
pass ─► exit 0      fail ─► exit 1 with solver counterexample
```

### Components

| Component | Responsibility |
|---|---|
| `ringlean-macros` (proc-macro crate) | `#[lean_verify(...)]` and `#[lean_assume(...)]`. They pass code through unchanged in normal builds and emit `ringlean::` tool attributes under `cfg(ringmpsc_lean)`. |
| `cargo-ringlean` (driver) | Find annotated functions, take their MIR before optimization, run the exporter, run `lake build`, report results. |
| `ringlean-exporter` | Translate MIR statements and terminators one to one into `RingMir` terms. Atomic loads become unconstrained values limited by the `lean_assume` premises. `UnsafeCell::get` reads of the caches are handled the same way. |
| `RingMir` (Lean library) | Semantics for wrapping `u64`/`usize` arithmetic, `&`, `min`, `saturating_sub`, and opaque `T`; obligation rules for `from_raw_parts[_mut]`, `ptr.add` and `Assert`; a control-flow model. |
| `ring_vc` (Lean tactic) | Generates the goals for each function from the obligation sites and closes them with `omega` / `bv_decide`, like cuda-oxide's `mir_vc`. |

### Integration with existing skills

- **`/verify`** gets a fourth stage, `cargo ringlean -p ringmpsc-rs`, next to concurrency, spec and invariants.
- **`/verify-invariants`** accepts a Lean theorem named `inv_*` as coverage for an INV ID, alongside the `debug_assert!` macros.
- **`cargo ringlean export <fn>`** prints the MIR and the generated Lean, like `cargo oxide lean-export`, for debugging failed proofs.

### Checking the exporter itself

As in cuda-oxide, the exporter is trusted and likely to have bugs at first. We have a tool cuda-oxide doesn't: **Lean models can be run.** Execute the generated `RingMir` model on the same Quint ITF traces that `crates/ringmpsc/tests/quint_mbt.rs` replays (`RingSPSC.qnt`, same seeds), and compare its state with the real `Ring` after each step. A mismatch means the exporter's translation is wrong. This reuses the existing MBT setup to test the exporter, much as #34 used it to tie the Verus ring to production.

### Cost

This is a significant project: a compiler tool tied to a nightly version, plus a Lean semantics library and a tactic to maintain. It is justified only if Phase 0 makes clear that proving the unsafe glue is worth more than Miri and review already provide.

---

## 6. Trusted code

| Phase 0 | Phase 1 |
|---|---|
| Charon and Aeneas (translation of `window.rs`) | `rustc` up to MIR as built |
| That the unsafe code uses the window functions correctly (review, Miri) | `ringlean-exporter` (checked against ITF traces, see section 5) |
| The protocol premises (Verus, Quint, MBT) | The protocol premises (Verus, Quint, MBT) |
| That the orderings are strong enough (loom, Miri) | That the orderings are strong enough (loom, Miri) |
| `rustc` and LLVM codegen | `rustc` optimization and LLVM codegen |

---

## 7. Where this fits

This extends the current division of labor between the Verus-based verified ring and the other tools:

| Concern | Verus | Quint | MBT / proptest | Miri | loom | **ringlean** |
|---|---|---|---|---|---|---|
| Protocol invariants | ✅ every capacity | ✅ bounded | ✅ production | | | assumed |
| Index and window arithmetic | ✅ verified ring | ⬜ unbounded integers | ✅ production, sampled | | | ✅ **production, all inputs** |
| Unsafe slice bounds | ✅ verified ring | | | ✅ production, sampled | | Phase 0: via cited theorems; Phase 1: ✅ production |
| Memory orderings | SeqCst only | | | ✅ | ✅ replica | |
| Liveness | | ✅ bounded | | | | |

The new column is the only entry that says **production, all inputs**.

---

## 8. Open questions

1. **Toolchains.** ~~Can Charon's nightly pin coexist with the Verus pin (Rust 1.99.1)?~~ Yes: Charon runs on its own nightly with its own target directory, and `crates/ringmpsc/lean/` is a standalone Lake project outside the Cargo workspace.
2. **Aeneas and `u64 → usize` casts.** ~~Should the theorems assume a 64-bit `usize`?~~ Resolved: Aeneas leaves the width abstract, and the theorems hold for both 32-bit and 64-bit.
3. **CI cost.** Lean build time grows with the code verified. Should it run on every PR, or only on changes to `ring.rs`, `stack_ring.rs`, `reservation.rs` or `window.rs`?
4. **The verified ring.** If phase 7 (#26) replaces production with `verus/ringmpsc-verified`, most of this proposal's reason to exist goes away. Decide #26 first.

## References

- [Provably safe GPU kernels](https://nihalpasham.github.io/posts/provably-safe-gpu-kernels/): cuda-oxide's Lean 4 integration
- The Verus-based verified ring on the main branch: `verus/ringmpsc-verified`
- [`crates/ringmpsc/spec.md`](crates/ringmpsc/spec.md): INV IDs
- [`docs/VERIFICATION_ARCHITECTURE.md`](docs/VERIFICATION_ARCHITECTURE.md), [`docs/FORMAL_VERIFICATION_WORKFLOW.md`](docs/FORMAL_VERIFICATION_WORKFLOW.md)
- Charon and Aeneas: <https://github.com/AeneasVerif/charon>, <https://github.com/AeneasVerif/aeneas>
