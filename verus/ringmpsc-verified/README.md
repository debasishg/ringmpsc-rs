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
| `readable_avail` | `readable`'s fast-path decision: refresh if `cached_tail <= head`, else `cached_tail - head` | correct for every pair of values, including `cached_tail < head`. See [Bugs this found](#readable-after-consume_batch-17-fixed). |

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

The two models agree on state, actions and the safety invariants. Both guard the producer by `cached_head`, the only view of `head` it has on the fast path, and both carry cache soundness (`cachesLag` in Quint, `caches_lag` here). One thing this model has that the Quint model doesn't: **batches and partial commits.** That covers `reserve(n)`, `commit(k <= n)`, `abandon()`, and reading a window then advancing by less.

The models are maintained by hand. If they ever disagree on safety, this proof is the authority. Quint remains the only liveness check, and the source of the traces `quint-connect` replays against the real `Ring`.

### `src/slots.rs` and `src/ring.rs`: a verified exec ring (phase 3, [#22](https://github.com/debasishg/ringmpsc-rs/issues/22))

`slots.rs` is the protocol with the ring's memory in it: a VerusSync machine, `SpscSlots<T>`, whose `storage_map` holds a `PointsTo` permission token for every slot. Tokens are keyed by sequence number over the window `[head, head + cap)`, and sequence number `s` uses cell `cells[s % cap]`. A side can touch a slot only by withdrawing its token through a transition, and the transitions only allow what the protocol allows:

| Transition | Code | Token movement |
|---|---|---|
| `produce_start(n)` | `reserve(n)` | withdraw the uninitialized tokens for `[tail, tail + n)`, if `tail + n <= cached_head + cap` |
| `produce_end(k)` | `commit_up_to(k)`: `tail.store(tail + k)` | deposit them back, the first `k` initialized and the rest empty; append the `k` values to the producer's ghost history `produced` |
| `consume_start(n)` | `readable` | withdraw the initialized tokens for `[head, head + n)`, if `head + n <= cached_tail`; record their values as `held_values` |
| `consume_end(k)` | `release(k)`: `head.store(head + k)` | deposit the first `k` back empty under `s + cap` (the next sequence number for that cell), the rest untouched under `s`; append the `k` values to `consumed` |
| `refresh_head`, `refresh_tail` | the slow paths | update the caches |

The invariant `window` says every sequence number in the window has its token either in storage (initialized with `produced[s]` if published, uninitialized if not) or with the side that withdrew it, never both. The invariant `fifo` says `consumed` is always a prefix of `produced`. Both are proven inductive for every capacity.

The contents are two histories, not one queue, so that each side owns the one it updates. A single queue would be updated by both sides, and its token couldn't live with either thread. Each side's start transition is keyed by its own history's length (equal to `tail` or `head` by invariant), so it needs only tokens that side owns.

`ring.rs` is an exec `Ring<T>` built on it, single-threaded, with the same design as the production ring: unbounded `u64` sequence numbers, `arith::slot_index` for `seq & mask`, and a cached head and tail with fast and slow paths. Each slot is a `vstd` `PCell<T>`, which is `UnsafeCell<MaybeUninit<T>>` underneath, the production layout. It proves:

| | Proven |
|---|---|
| **No uninitialized read** | `take` needs an initialized token, and the protocol hands one out only for a published sequence number |
| **No double free** | a slot's token is unique, and `take` leaves it uninitialized until the producer writes again |
| **FIFO** | `push` appends to the view; `pop` returns `view[0]`; `consume_up_to(max)` returns `view.take(k)` for `k = min(max, len)` |
| **Completeness** | `push` succeeds whenever the ring isn't full |
| **No sequence-number overflow** | `push` refuses once `tail == u64::MAX` (about 58 years at 10 billion items a second) instead of wrapping: INV-SEQ-03 as a checked fact |

`pop` mirrors `readable`'s fast path as fixed in #17: it reads by the cached tail only when `cached_tail > head`. With the old `wrapping_sub` check it fails verification (mutation M7 below).

`Ring` holds every token itself, so it is single-threaded; its `push` and `pop` are batches of one. `spsc.rs` (below) splits the tokens between two threads and exposes the batches.

Dropping a `Ring` drops the items still in it, as the production ring does (see [Trusted code](#trusted-code)).

### `src/spsc.rs`: a verified concurrent ring (phase 4, [#23](https://github.com/debasishg/ringmpsc-rs/issues/23))

`channel(ring_bits)` returns a `Producer` and a `Consumer` that run on different threads. They share the cells and two `vstd` atomics through an `Arc`:

| Atomic | Holds | Invariant |
|---|---|---|
| `tail` | the protocol's `tail` token | the token's value is the atomic's value |
| `head` | the protocol's `head` token | the same |

Each half owns the rest of its side's tokens:

| Half | Tokens |
|---|---|
| `Producer` | `cached_head`, `producing`, `produced` |
| `Consumer` | `cached_tail`, `consuming`, `consumed` |

Neither half is `Clone`, and every method takes `&mut self`. Because the tokens can't be duplicated, there is provably one producer and one consumer, the property #12 enforces in the production crate.

Every protocol step that touches a shared cursor happens inside an atomic operation on it (`atomic_with_ghost!`). These are the same points where the production code does its `load`s and `store`s:

| Code | Atomic operation | Protocol step |
|---|---|---|
| `push`, slow path | `head.load()` | `refresh_head` |
| `push`, publish | `tail.store(tail + 1)` | `produce_end` |
| `pop`, slow path | `tail.load()` | `refresh_tail` |
| `pop`, release | `head.store(head + 1)` | `consume_end` |

So the proofs hold under **every interleaving** of the two threads:

| | Proven |
|---|---|
| **No data race on a slot** | a slot is written only while the producer holds its token, and read only while the consumer does |
| **No uninitialized read, no double free** | as for `Ring`, now across threads |
| **FIFO across threads** | `push` appends to `produced`; `pop` appends what it returns to `consumed`; `fifo_holds(&producer, &consumer)` proves `consumed` is a prefix of `produced` |
| **One producer, one consumer** | each side's tokens exist once |

`Consumer::consume_up_to(max)` returns items in order and extends `consumed` by exactly them.

#### Batches (phase 5, [#24](https://github.com/debasishg/ringmpsc-rs/issues/24))

| Producer | Proven |
|---|---|
| `reserve(n)` | grants up to `n` slots, never more than the free space (by `cached_head`, reloading `head` when the cache says full) |
| `write_next(v)` | fills the next reserved slot; `pending()` grows by `v` |
| `commit_up_to(k)`, `commit()` | publishes the first `min(k, filled)` written items with one `tail.store`; `produced` grows by exactly those; written items past `k` are dropped |
| `abandon()` | publishes nothing and drops what was written (a `Reservation` dropped without committing) |
| `push(v)` | a reservation of one |

| Consumer | Proven |
|---|---|
| `readable()` | holds every published item (#17's `cached_tail > head` fast path) |
| `get(i)` | borrows held item `i` **in place**, no copy: `*get(i) == held_view()[i]` |
| `release(k)` | moves the first `k` held items out, in order, with one `head.store`; the rest stay in the ring |
| `advance(k)` | `release(k)`, dropping them |
| `recv(out, max)` | copies up to `max` items out (`T: Copy`) through `get`, then consumes them |
| `pop()`, `consume_up_to(max)` | built on the above |

What #24 planned and this deliberately does **not** provide:

- **A writable `&mut [MaybeUninit<T>]` slice.** Nothing could show which slots the caller wrote through it, and committing an unwritten slot is undefined behavior. The production `Reservation::commit` has exactly that hole (see [Bugs this found](#bugs-this-found)). The verified API writes through `write_next`, which tracks it.
- **A `&[T]` spanning several slots.** `vstd`'s `PCell` is not `#[repr(transparent)]` and exposes no pointer, so building one would rest on a layout Rust doesn't guarantee. `get(i)` gives the same zero-copy access per element.

**Orderings:** `vstd` atomics are sequentially consistent, and Verus reasons about them that way. The proof shows the protocol is race-free. It does not show that the production code's `Relaxed`/`Acquire`/`Release` are enough; that stays with loom and Miri. Measuring what SeqCst costs is part of phase 7 ([#26](https://github.com/debasishg/ringmpsc-rs/issues/26)).

### `src/channel.rs`: a verified MPSC channel (phase 6, [#25](https://github.com/debasishg/ringmpsc-rs/issues/25))

`Channel::new(ring_bits, max_producers)` builds one verified `spsc` ring per producer, as the production `Channel` does. Handles are shared with `clone_handle` and can be used from any thread. The verified part is the **handout**, which moves exec values out of shared memory through `PCell`s whose permissions live in atomic invariants:

| Operation | Mechanism | Proven |
|---|---|---|
| `register()` | Each ring's `Producer` half sits in a `PCell`. The `producer_count` atomic's invariant holds the permissions for the unclaimed ones, `[count, N)`. A compare-and-swap from `c` to `c + 1` withdraws permission `c`. | each ring is handed out **at most once**, in order, and never past `max_producers`; the producer returned is well-formed and belongs to ring `id` |
| `consumer()` | All N `Consumer` halves sit in one `PCell`. The `consumer_taken` flag's invariant holds its permission while the flag is `false`. Claiming swaps in `true` and withdraws it. | **at most one** `ChannelConsumer` exists at a time: #12's claim, with its exclusivity proven |
| releasing (dropping the `ChannelConsumer`) | puts the halves back, then stores `false` along with the permission | the claim can be taken again, on any thread |
| `consume_all(max_per_ring)` | drains each ring through its `Consumer` half, ring 0 first | each producer's items come out in the order it sent them (INV-CH-03): every ring's `consumed` only grows by appending |

`ChannelConsumer::ring(i)` exposes ring `i`'s consumer half, so `spsc::fifo_holds` states per-producer FIFO against that producer.

When the last handle goes away, the channel drops what it still owns: the unregistered producer halves, and, if unclaimed, the consumer halves (which drop the items left in their rings). The verified `drain` does this by registering and claiming through the same protocol. Without it, those halves would stay in their `PCell`s and leak. Miri caught that during development.

### Trusted code

Everything above is verified, except these small pieces outside `vstd` that Verus takes on trust:

| What | Why it is trusted | Why it is sound |
|---|---|---|
| `arc_clone` in `spsc.rs`, one line, `#[verifier::external_body]` | `vstd` has no spec for `Arc::clone` | its spec says only that the clone points at the same value |
| `impl Drop` for `Consumer`, `Ring`, `ChannelConsumer` and `ChannelShared`, plain Rust outside `verus!` | Verus has no way to let `Drop::drop` assume `wf()` short of converting the types to type invariants | each is one call to a verified function (`consume_up_to`, `release`, `drain`) whose precondition `wf()` holds for every value safe code can obtain: fields are private, constructors establish it, and every method preserves it |

`Consumer`'s `Drop` drops the items published so far. Items the producer publishes after that are leaked (safely).

### The proofs aren't vacuous

Each of these deliberate breakages makes verification fail, with the error shown:

| Mutation | Verus reports |
|---|---|
| `readable_avail` with the pre-#17 logic (`cached_tail.wrapping_sub(head) == 0` decides the refresh) | postcondition not satisfied |
| `reserve` allows one slot more than `cached_head` permits | invariant `reservation_in_free_space` fails |
| `commit(k)` without `k <= n` | invariant `bounded_count` fails |
| `contiguous` as `n.min(cap - idx + 1)` | postcondition not satisfied |
| `slot_index` masks with `cap` and not `cap - 1` | postcondition not satisfied |
| `refresh_tail` overshoots: `cached_tail = tail + 1` | invariant `caches_lag` fails |
| M7: `pop` uses the pre-#17 check, `cached_tail.wrapping_sub(head) == 0` | precondition not satisfied (`consume_start` needs `head < cached_tail`) |
| M8: `push` without the slow path (no `head` reload) | postcondition not satisfied ("not full implies `Ok`") |
| M9: `push` writes slot `tail + 1` | precondition not satisfied (the token belongs to another cell) |
| M10: `pop` takes the item twice, a double free | precondition not satisfied (the second `take` needs an initialized token) |
| M11: the token goes back under `head` and not `head + cap` | invariant `window` fails |
| M12: `push` without the `tail == u64::MAX` guard | possible arithmetic overflow |
| C1: the producer publishes `tail` before writing the slot | precondition not satisfied (`produce_end` needs an initialized token) |
| C2: the consumer releases `head` before taking the item | rejected by the borrow checker: the token was already given back |
| C3: the `tail` atomic's invariant doesn't tie the token to the value | postcondition not satisfied (the consumer's refreshed cache can't be trusted) |
| C4: the consumer uses the pre-#17 check | precondition not satisfied, and a possible underflow |
| C5: the producer's space check is off by one (`>` for `>=`) | precondition not satisfied (`produce_start` needs room) |
| C6: `consume_start` doesn't record the item in `consumed` | invariant `cursors` fails |
| H1: `register` withdraws the next ring's permission | precondition not satisfied; the atomic's invariant can't be restored |
| H2: `register` uses `store` and not compare-and-swap (two threads could get one ring) | precondition not satisfied; the atomic's invariant can't be restored |
| H3: the consumer claim ignores the flag's previous value | precondition not satisfied (no permission while claimed) |
| H4: `release` doesn't put the consumer halves back | the atomic's invariant can't be restored |
| H5: the producer bound is off by one (`>` for `>=`) | precondition not satisfied, and a possible overflow |
| B1: `commit_up_to` ignores `k` and publishes everything written | postcondition not satisfied |
| B2: commit keeps the written items past `k` in their slots | loop invariant not satisfied |
| B3: `reserve` grants `n` regardless of free space | precondition not satisfied (`produce_start` needs room) |
| B4: `release` returns a consumed slot under `s`, not `s + cap` | assertion failed |
| B5: `write_next` writes the slot after the next one | assertion failed (the token belongs to another cell) |
| B6: `produce_end` doesn't require the published slots to be written (the production bug) | the state machine's `window` invariant can't be proven |

## Bugs this found

### `Reservation::commit` publishes unwritten slots (production; not yet filed)

The production `Reservation::as_mut_slice()` hands out `&mut [MaybeUninit<T>]`, and `commit()` is safe and publishes every reserved slot. Nothing makes the caller write them first:

```rust
let channel = Channel::<Box<u64>>::new(Config::new(2, 1, false));
let mut producer = channel.register().unwrap();
producer.reserve(1).unwrap().commit();          // nothing written
channel.consumer().unwrap().consume_all_owned(drop); // reads an uninitialized Box
```

Miri: `Undefined Behavior: ... memory is uninitialized`. It is reachable from safe code on the main heap ring, the same class as #15 (fixed for `StackRing`). Phase 5 surfaced it: the verified `produce_end` requires an initialized token for every slot it publishes, and mutation B6 shows the invariant fails without that requirement.

### `readable()` after `consume_batch*` ([#17](https://github.com/debasishg/ringmpsc-rs/issues/17), fixed)

The state machine's fast-path read needs `head < cached_tail`, and `cached_tail <= tail` alone doesn't give it. The code didn't maintain `head <= cached_tail` either:

- `consume_batch*` and `consume_up_to*` advance `head` from a fresh load of `tail` without updating `cached_tail`, and `advance` moves `head` without loading `tail` at all.
- A later `readable()` computed `cached_tail.wrapping_sub(head)`, which wraps to a huge count when `cached_tail < head`. The `avail == 0` refresh was skipped, and `readable` returned a slice over unwritten slots.

```rust
let mut ring = Ring::<u64>::new(Config::new(2, 1, false));
ring.push(7);
ring.consume_batch(|_| {});      // head = 1, cached_tail still 0
ring.readable();                 // was: Some(slice of 3 unwritten slots); now: None
```

Natively this returned garbage values. Under Miri it was `Undefined Behavior: ... memory is uninitialized`. `recv()` and `StackRing::peek` reached the same path. The Quint model didn't catch this, because it compares `ct > h` over unbounded integers.

**The fix** compares instead of subtracting: `readable` and `peek` refresh whenever `cached_tail <= head`. Keeping `cached_tail` current in the consume paths would not have been enough on its own, because `advance(n)` moves `head` without any Acquire load of `tail`, so it has no synchronized value to cache. `readable_avail` mirrors the fixed decision and verifies with no precondition. The old logic fails its postcondition.

## Not covered (yet)

- **Slices.** A writable `&mut [MaybeUninit<T>]` reservation and a multi-slot `&[T]`; see phase 5 above for why.
- **The production ring.** `ring.rs` and `spsc.rs` are verified rings with the production design, not the production code. Whether it replaces `crates/ringmpsc`'s `Ring` is decided in [#26](https://github.com/debasishg/ringmpsc-rs/issues/26).
- **Memory orderings.** Verus treats atomics as sequentially consistent, so it can't show `Relaxed`/`Acquire`/`Release` suffice. That stays with loom and Miri.
- **Liveness.** That stays in `RingSPSCLiveness.qnt`.
- `Channel::close`/`is_closed`, metrics, `StackRing`, allocators, NUMA.

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

Expected output: `verification results:: 94 verified, 0 errors`.

The verified rings also compile as ordinary Rust, with the ghost code erased.

- `tests/fifo.rs` runs `Ring`, including a 20,000-operation comparison against a `VecDeque`.
- `tests/spsc.rs` runs `channel` on two real threads: 100,000 items in order, batches with `consume_up_to`, and drop counting.
- `tests/batches.rs` runs the batch API: reserve capped by free space, partial commits and abandon (with drop counting), in-place `get` and `release`, `recv`, and random batch sizes on both sides across two threads.
- `tests/channel.rs` runs `Channel`: concurrent registration (distinct ids, none past the limit), per-producer FIFO with 4 producer threads, the exclusive consumer claim released across threads, and teardown dropping every remaining item (under Miri, also a leak check).

All pass natively and under Miri. For the threaded tests, use preemption so Miri explores interleavings:

```sh
CARGO_TARGET_DIR=target/plain cargo test
CARGO_TARGET_DIR=target/miri MIRIFLAGS="-Zmiri-preemption-rate=0.1" cargo +nightly miri test
```

Cargo caches the result: if nothing changed since the last run, you get only `Finished`. A plain `cargo build` or `cargo test` (which compiles, with the ghost code erased) in the default `target/` can also make the next `cargo verus` skip verification. That's why the commands above use separate target directories. Run `cargo clean` to force a fresh check.
