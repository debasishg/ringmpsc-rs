# ringmpsc-verified

Machine-checked proofs for the `ringmpsc` SPSC ring, using [Verus](https://github.com/verus-lang/verus).

This crate is a standalone workspace. Verus pins its own Rust toolchain, and keeping it out of the root workspace means the main crates' builds and CI never depend on it.

## What is verified

### `src/arith.rs`: the ring's arithmetic, as exec code

Each function has the same body as the corresponding expression in `crates/ringmpsc/src/ring.rs` or `config.rs`. The contract states what the ring relies on it for.

| Function | Mirrors | Proven |
|---|---|---|
| `capacity` | `Config::capacity`: `1 << ring_bits` | `2 <= cap <= 2^20` for every accepted `ring_bits` |
| `slot_index` | `(seq as usize) & mask` | in bounds, and equal to `seq % cap` (INV-MEM-02: masking is modulo) |
| `len` | `tail.wrapping_sub(head) as usize` | under INV-SEQ-01 the subtraction never wraps; the result is the exact item count |
| `space` | `capacity.saturating_sub(tail.wrapping_sub(head) as usize)` | exact free space, or 0 |
| `lemma_stale_head_underestimates_space` | the `reserve` fast path | space computed from a stale `cached_head` never exceeds the real space, so a fast-path "yes" is always right |
| `contiguous` | `n.min(capacity - idx)` | never past the buffer end, never more than asked, non-empty if anything was asked |
| `lemma_contiguous_slots_do_not_wrap` | `make_reservation`'s slice | sequence number `tail + k` lands on slot `idx + k`, so the slice covers exactly the reserved sequence numbers |
| `readable_avail` | `readable`'s fast path: `cached_tail.wrapping_sub(head)` | the item count, **only if `head <= cached_tail`**. See [A bug this found](#a-bug-this-found). |

### `src/protocol.rs`: the SPSC protocol, as a VerusSync state machine

This is the Verus counterpart of `crates/ringmpsc/tla/RingSPSC.qnt`. Quint model-checks that spec for `CAPACITY = 4, MAX_ITEMS = 8`. Here the invariants are proven **inductive**, so they hold for every capacity and every number of items.

| Quint action | Transition | Code |
|---|---|---|
| `producerReserveFast` | `reserve(n)` | `reserve_shared`, guarded by `cached_head` |
| `producerRefreshCache` | `refresh_head` | slow path: `head.load(Acquire)` |
| `producerWrite` | `commit(k)`, `abandon` | `commit_internal` (`commit`, `try_commit_n`, `commit_up_to`); a dropped `Reservation` |
| `consumerReadFast` | `read_cached(end)` | `readable` fast path |
| `consumerRefreshCache` | `refresh_tail` | slow path: `tail.load(Acquire)` |
| (none) | `read_fresh(end)` | `consume_batch*` / `consume_up_to*`, which load `tail` directly |
| `consumerAdvance` | `advance(k)` | `head.store(Release)`, `advance` |

**Invariants**, all proven inductive:

| Invariant | spec.md |
|---|---|
| `head <= tail <= head + cap` | INV-SEQ-01, INV-ORD-03 |
| `cached_head <= head`, `cached_tail <= tail` | soundness of both caches |
| a reservation `[tail, tail + n)` fits in the free space: `tail + n <= head + cap` | INV-INIT-03 |
| the consumer reads `[head, end)` with `end <= tail` | INV-INIT-01 |

**Properties** derived from them:

- `producer_and_consumer_slots_disjoint`: while a reservation is outstanding, no slot it covers is being read. This is the claim behind the `SAFETY` comments in `make_reservation` and `consume_batch`.
- `reserved_slots_distinct`: the `&mut [MaybeUninit<T>]` a reservation hands out never has two elements on the same slot.
- `reads_are_published`: the consumer only reads sequence numbers below `tail`.

Two things this model has that the Quint model doesn't:

- **Batches and partial commits.** `reserve(n)`, `commit(k <= n)`, `abandon()`, and reading a window then advancing by less.
- **Only what each side actually knows.** Quint's `producerWrite` checks space against the real `hd`, which the producer never reads on the fast path. Here `reserve` is guarded by `cached_head`, and `cached_head <= head` is what makes that sound.

### The proofs aren't vacuous

Each of these deliberate breakages makes verification fail, with the error shown:

| Mutation | Verus reports |
|---|---|
| `readable_avail` without `head <= cached_tail` | postcondition not satisfied |
| `reserve` allows one slot more than `cached_head` permits | invariant `reservation_in_free_space` fails |
| `commit(k)` without `k <= n` | invariant `bounded_count` fails |
| `contiguous` as `n.min(cap - idx + 1)` | postcondition not satisfied |
| `slot_index` masks with `cap` and not `cap - 1` | postcondition not satisfied |
| `refresh_tail` overshoots: `cached_tail = tail + 1` | invariant `caches_lag` fails |

## A bug this found

The state machine's fast-path read needs `head < cached_tail`, and `cached_tail <= tail` alone doesn't give it. The code doesn't maintain `head <= cached_tail` either:

- `consume_batch*` and `consume_up_to*` advance `head` from a fresh load of `tail` without updating `cached_tail`.
- A later `readable()` computes `cached_tail.wrapping_sub(head)`, which wraps to a huge count when `cached_tail < head`. The `avail == 0` refresh is skipped, and `readable` returns a slice over unwritten slots.

```rust
let mut ring = Ring::<u64>::new(Config::new(2, 1, false));
ring.push(7);
ring.consume_batch(|_| {});      // head = 1, cached_tail still 0
ring.readable();                 // Some(slice of 3 unwritten slots)
```

Natively this returns garbage values. Under Miri it is `Undefined Behavior: ... memory is uninitialized`. `recv()` reaches the same path. In debug builds it panics on `advance`'s `debug_assert!`; in release it reads uninitialized memory too. The Quint model doesn't catch this, because it compares `ct > h` over unbounded integers.

The fix belongs in `ring.rs`: test `cached_tail > head`, or have the consume paths keep `cached_tail` current. This crate doesn't change the ring. It records the precondition in `readable_avail`.

## Not covered (yet)

- **Slot contents and memory.** The protocol says *which* slots each side may touch. Tying that to real memory takes the exec ring, with `PCell` slots and `PointsTo` permission tokens held by the protocol.
- **Atomics and threads.** `head`/`tail` as `vstd` atomics whose invariants hold the protocol's tokens.
- **Memory orderings.** Verus treats atomics as sequentially consistent, so it can't show `Relaxed`/`Acquire`/`Release` suffice. That stays with loom and Miri.
- **Liveness.** That stays in `RingSPSCLiveness.qnt`.
- `Channel`, `StackRing`, allocators, NUMA.

## Running

Verus is distributed as a prebuilt release, not through crates.io or Homebrew.

1. Download a release from <https://github.com/verus-lang/verus/releases>. This crate was verified with **0.2026.10.18.7d82994**; `vstd` is pinned to match in `Cargo.toml`.
2. Put its directory on `PATH` (it contains `verus` and `cargo-verus`).
3. Install the toolchain that release asks for. `verus --version` prints it; for this release it is `rustup install 1.99.1`. `rust-toolchain.toml` pins the same version.

Then, from this directory:

```sh
cargo verus focus     # verify this crate only (vstd is trusted)
cargo verus verify    # verify vstd too, then this crate
```

Expected output: `verification results:: 31 verified, 0 errors`.

Cargo caches the result: if nothing changed since the last run, you get only `Finished`. A plain `cargo build` (which compiles, with the ghost code erased) shares `target/` and can also make the next `cargo verus` skip verification. Run `cargo clean` to force a fresh check.
