# Why Lean, when we already have Quint, Verus, loom and Miri?

`ringmpsc` is already checked by four tools. This note explains what the Lean 4 proofs from [`RINGLEAN_DESIGN.md`](RINGLEAN_DESIGN.md) (Phase 0) add, what they don't, and whether they're worth what they cost.

**Short answer:** each tool answers a different question. The Lean proofs fill one narrow gap: **they're the only proof about the code that actually ships, for every input.** They cover the layer where most past memory-safety bugs were, and they replace none of the other tools.

## What each tool checks

| Tool | What it checks | On which code | For which inputs |
|---|---|---|---|
| **Quint** | the SPSC protocol: safety, deadlock, liveness | a separate hand-written model (`tla/RingSPSC.qnt`) | every state, but only up to capacity 4 and 8 items |
| **Verus** | protocol invariants, FIFO, memory safety | `verus/ringmpsc-verified`, a **reimplementation** of the ring | every input, every capacity |
| **loom** | the memory orderings are strong enough | `LoomRing`, a **replica** of the ring | every thread interleaving of small tests |
| **Miri** | undefined behavior: bad pointers, uninitialized reads, data races | the **production code** | only the executions the tests drive |
| **Lean (Phase 0)** | index and window arithmetic | the **production code** (`window.rs`, translated directly) | every input, every capacity |

The last two columns are what matter. Every tool except Lean either checks a copy of the code (Quint, Verus, loom) or checks only the cases a test happens to run (Miri). Lean is the only one that proves something about the production source for every input.

## Why that gap matters

Four of the five memory-safety bugs fixed during the Verus work (#14, #15, #17, #32) were arithmetic mistakes within one thread:

| Issue | Mistake |
|---|---|
| #14 | `advance(n)` could move `head` past `tail` |
| #15 | `StackRing::commit` published slots that were never written |
| #17 | `readable()` returned unwritten slots when its cached `tail` trailed `head` |
| #32 | `Reservation::commit` published slots that were never written |

None of the other tools could rule these out ahead of time:
- **Quint missed them.** Its model writes and publishes a slot in one step, and it only checks small sizes.
- **Verus would have caught them, but only in its own reimplementation.** Differential tests show the two rings behave alike on the inputs tested. Nothing proves the production ring computes its indices the same way.
- **Miri and the tests found them** only once someone wrote a test that reached the bad case.

Phase 0 moves all of that arithmetic into six pure functions in `crates/ringmpsc/src/window.rs` and proves them for every input:
- indexes stay inside the buffer
- windows have the right length
- a stale cached `head` never reports more free space than the real one
- the slots a reservation hands out never overlap slots the consumer hasn't read yet

The proofs cover both 32-bit and 64-bit `usize`. They also show none of the functions can panic.

The proofs are enforced. `crates/ringmpsc/lean/scripts/check.sh` runs in `/verify` and in CI (`.github/workflows/lean.yml`), and they can't silently fall out of date:
- **Regenerated every run.** The Lean model is rebuilt from `window.rs` each time, and CI fails if it differs from what's committed.
- **Only standard axioms.** No theorem may rely on `sorry`, a user-declared axiom, or `native_decide`.
- **Citations must be real.** Every `[lean: ...]` citation in a `// Safety:` comment must name a proved theorem.

To test that the proofs actually catch bugs, five mistakes were seeded in `window.rs` one at a time. `check.sh` failed on every one:
- an off-by-one in `write_window`
- an off-by-one in `free_space`
- `write_window` ignoring `n` (the #15/#32 kind of bug)
- `read_window` using `<` instead of `<=` (the #17 kind)
- a wrong mask in `slot_index`

## What Lean doesn't do

It doesn't replace any of the other tools:

| Concern | Who covers it | Why not Lean |
|---|---|---|
| The protocol across two threads | Verus, Quint, Quint trace replay | Lean *assumes* `head ≤ tail ≤ head + capacity` and that caches never lead. It proves each thread's arithmetic given those facts. |
| Memory orderings (`Relaxed`/`Acquire`/`Release`) | loom, Miri | Lean has no model of the C++/Rust memory model. |
| Liveness (every item is eventually consumed) | Quint | Phase 0 proves properties of individual functions, not progress over time. |
| The `unsafe` code that uses the arithmetic | review, Miri | Lean proves `write_window` returns a correct window, not that `make_reservation` passes it correctly to `from_raw_parts_mut`. The `// Safety:` comments *cite* the theorems; Phase 1 would *prove* the `unsafe` code itself. |
| `u64` sequence wrap-around | INV-SEQ-03 (an assumption) | The proofs assume sequence numbers never wrap. At 10⁹ messages per second, a `u64` takes about 584 years to wrap. |

## Is it worth it?

**For:**
- **It's the only proof tied to the code that ships,** and it can't drift from that code, because CI regenerates and re-proves it on every relevant change.
- **It covers the layer where most past bugs were:** arithmetic within one thread.
- **It's cheap to run:** about a minute in CI with warm caches, and only on changes to the relevant files.

**Against:**
- **It hasn't found a new bug yet.** The `StackRing` pointer-provenance bug (#36) was found by Miri, not Lean. So far Lean prevents regressions; it hasn't discovered anything.
- **It adds toolchain cost:** a pinned Aeneas release, Charon's own nightly Rust, Lean, and a few GB of Mathlib cache.
- **It adds trust in new tools:** Charon and Aeneas must translate `window.rs` correctly. That is less risky for safe, pure arithmetic than for `unsafe` code, but it isn't nothing.
- **The proved code is small:** six short functions.
- **It may become unnecessary.** If #26 decides to replace the production ring with the Verus-verified one, Verus covers this layer directly.

**Recommendation.** Keep Phase 0. Its value is that changes to this code can't quietly break the arithmetic behind the `unsafe` code, and that check already runs in CI. Build Phase 1 (proving the `unsafe` code itself, via a MIR-to-Lean driver) only if #26 keeps the current production ring. If the Verus ring replaces it, Verus already covers that code, and Phase 1 isn't needed.

## See also

- [`RINGLEAN_DESIGN.md`](RINGLEAN_DESIGN.md): the design, Phase 0 status, and the Phase 1 proposal
- [`crates/ringmpsc/lean/README.md`](crates/ringmpsc/lean/README.md): setup, the list of theorems, how to add a function
- [`docs/VERIFICATION_ARCHITECTURE.md`](docs/VERIFICATION_ARCHITECTURE.md): how all the verification layers fit together
