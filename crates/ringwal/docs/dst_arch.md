# Deterministic Simulation Testing for ringwal — Architecture & Experience Report

This document describes the design, implementation, and outcomes of adding Deterministic Simulation Testing (DST) to ringwal, a Write-Ahead Log built on the ring-decomposition MPSC channel.

---

## 1. Motivation

Write-ahead logs sit at the durability boundary: the exact place where software promises meet hardware reality. The hardest bugs in WAL implementations live in the gaps between what the application *thinks* happened and what the disk *actually* recorded. These bugs share three characteristics:

1. **They require specific fault timing** — a crash between `write()` and `fsync()`, a partial write at exactly the segment boundary, a torn page during rotation.
2. **They are non-deterministic under real I/O** — standard integration tests exercise happy paths; real-world failures depend on OS scheduling, disk firmware, and power loss timing.
3. **They are invisible until data loss** — conventional unit tests cannot observe that an acknowledged commit was silently not durable.

ringwal's architecture — multiple concurrent writers feeding into a background flusher that batches `write()` + `fsync()` calls across segment files — has a large surface area for such bugs. DST was introduced to make this surface area explorable, reproducible, and testable within seconds rather than months of production observation.

---

## 2. Key Design Decisions

| Decision | Rationale |
|----------|-----------|
| **Separate `ringwal-sim` crate** | Zero simulation code in the production binary. Also validates that ringwal's public API is sufficient for external consumers — the simulator exercises only public types. |
| **Generic parameterization over feature flags** | `cfg(test)` or feature-gated simulation code means DST may not test the exact same code paths as production. Generics guarantee identical logic; only the I/O leaf calls differ. |
| **Synchronous trait methods** | ringwal uses `std::fs` (blocking I/O), not `tokio::fs`. The trait methods mirror this — `fn write_all()`, `fn sync_all()` — keeping the abstraction honest. |
| **`RefCell`-based SimIo** | DST is strictly single-threaded. Using `Mutex` would add contention overhead and hide potential re-entrancy bugs. `RefCell` panics immediately if a borrow violation occurs, which is a feature, not a cost, for simulation. |
| **Oracle-based verification** | The `CommitOracle` maintains an independent ground truth of which transactions were acknowledged. After crash + recovery, the oracle comparison is the strongest possible correctness check: it doesn't trust the WAL's own metadata. |
| **Seeded RNG for full reproducibility** | A single `u64` seed determines every fault injection decision, workload step, and timing advance. On failure, the test prints the seed so the exact scenario can be replayed indefinitely. |
| **`current_thread` tokio runtime per test** | The tokio `current_thread` flavour gives deterministic task scheduling: `.await` points yield control in a predictable order. This eliminates non-determinism from the async runtime, while still exercising real `async/await` codepaths. Multiple tests can run in parallel since each owns its own runtime and `SimIo`. |
| **DST scoped to Full/DataOnly/None sync modes** | Pipelined sync modes use `tokio::spawn_blocking` and `std::thread::spawn`, which escape the single-threaded runtime. These require a `TaskSpawner` abstraction (future Phase 6). |
| **No madsim dependency** | ringwal has zero networking. `madsim` and `turmoil` are designed for distributed systems with network partitions. A custom `SimIo` targeting the 15+ filesystem operations ringwal actually uses is simpler and sharper. |

---

## 3. Architecture

### 3.1 Crate Separation

```
┌─────────────────────────────────────────────────────────────────┐
│                     ringwal (production)                        │
│                                                                 │
│  ┌──────────────┐  ┌──────────────┐  ┌────────────────────┐    │
│  │  src/io/     │  │  Wal<IO>     │  │  recover<K,V,IO>() │    │
│  │  mod.rs      │  │  Segment<IO> │  │  checkpoint<IO>()  │    │
│  │  ├ IoEngine  │  │  SegmentMgr  │  │  truncate<IO>()    │    │
│  │  ├ FileHandle│  │  <IO>        │  │                    │    │
│  │  ├ ReadHandle│  │              │  │                    │    │
│  │  real.rs     │  │              │  │                    │    │
│  │  └ RealIo    │  │              │  │                    │    │
│  └──────────────┘  └──────────────┘  └────────────────────┘    │
│                                                                 │
│         IO = RealIo (default) │ IO = SimIo (testing)           │
└────────────────────────────────┼────────────────────────────────┘
                                 │ depends on public API only
                                 ▼
┌─────────────────────────────────────────────────────────────────┐
│                   ringwal-sim (test/dev only)                   │
│                                                                 │
│  ┌────────────────────────────────────────────────────────┐     │
│  │ RingwalSimulator                                       │     │
│  │  ├── SimIo: IoEngine   (in-memory FS, 3-tier writes)  │     │
│  │  │    ├── FaultConfig   (failure probabilities)        │     │
│  │  │    └── SimClock      (deterministic monotonic time) │     │
│  │  ├── CommitOracle       (ground truth tracker)         │     │
│  │  └── SmallRng           (seeded, deterministic)        │     │
│  └────────────────────────────────────────────────────────┘     │
│                                                                 │
│  tests/dst_tests.rs  ← 8 property tests × 1000 seeds           │
└─────────────────────────────────────────────────────────────────┘
```

The physical crate boundary is load-bearing: `ringwal-sim` depends on `ringwal` as a path dependency and imports only public types (`Wal`, `WalConfig`, `Transaction`, `WalWriter`, `recover`, etc.). If the public API cannot express something the simulator needs, that's a signal to fix the API — not to reach into `pub(crate)` internals.

### 3.2 SimIo: The Three-Tier Write Model

Real operating systems have layered write buffers. `SimIo` models this faithfully:

```
                Application
                    │
                    ▼
            ┌──────────────┐
            │ write_buffer  │  ← write_all() appends here
            │  (Vec<u8>)    │  ← LOST on crash
            └──────┬───────┘
                   │ flush()
                   ▼
            ┌──────────────┐
            │ kernel_buffer │  ← flush() promotes here
            │  (Vec<u8>)    │  ← LOST on crash
            └──────┬───────┘
                   │ sync_all() / sync_data()
                   ▼
            ┌──────────────┐
            │   durable     │  ← sync promotes here
            │  (Vec<u8>)    │  ← SURVIVES crash
            └──────────────┘
```

Each `SimFile` in the in-memory `HashMap<PathBuf, SimFile>` maintains these three tiers independently. Operations map to tier transitions:

| Operation | Effect |
|-----------|--------|
| `write_all(data)` | Appends to `write_buffer` |
| `flush()` | Drains `write_buffer` → `kernel_buffer` |
| `sync_all()` / `sync_data()` | Flushes first, then promotes `kernel_buffer` → `durable` |
| `crash()` | Clears `write_buffer` and `kernel_buffer` across ALL files; only `durable` survives |

This is the key insight that makes DST powerful: the model precisely captures the durability gap that causes real-world data loss.

### 3.3 Fault Injection

`FaultConfig` defines five independent fault rates (each `0.0..=1.0`):

| Fault | What it simulates |
|-------|-------------------|
| `write_fail_rate` | `write_all()` returns `io::Error` — disk full, I/O error |
| `fsync_fail_rate` | `sync_all()` / `sync_data()` returns error — data stays in kernel buffer, NOT promoted to durable |
| `partial_write_rate` | Write truncated to random shorter length — torn write from crash mid-I/O |
| `crash_probability` | Per-operation chance that `crash()` fires after the operation completes |
| `read_fail_rate` | `open_read()` fails even if file exists — transient FS errors |

Each fault decision is made by a seeded `SmallRng` so the entire fault sequence is reproducible from a single `u64` seed.

### 3.4 CommitOracle

The oracle is the ground truth for verification:

```
     WAL lifecycle                             Oracle
     ─────────────                             ──────
     tx.commit(&writer).await  ──── Ok ────►  oracle.record_commit(tx_id, entries)
                               ──── Err ───►  (not recorded)
     
     crash_and_recover()        
     
     verify_recovery(oracle, recovered)
       ├── lost:    oracle has, recovery doesn't  →  DURABILITY BUG
       └── phantom: recovery has, oracle doesn't  →  ATOMICITY BUG
```

The oracle records a commit **only** when `tx.commit()` returns `Ok`. If a fault causes the commit to return `Err`, the oracle stays silent — even if the data might have reached durable storage. This is conservative by design: it means "lost commit" detections are true positives.

### 3.5 Workload Driver

`RingwalSimulator::run_workload(steps)` generates a random sequence of operations using weighted probabilities:

| Weight | Operation |
|--------|-----------|
| 15% | Register a new writer (up to `max_writers`) |
| 65% | Create a transaction with 1–5 inserts, then commit |
| 10% | Create a transaction and abort it |
| 10% | Advance the simulated clock by 1–60 seconds |

The workload interacts with the WAL exclusively through its public API: `Wal::open()`, `factory.register()`, `Transaction::new()`, `tx.insert()`, `tx.commit()`, `tx.abort()`, `wal.shutdown()`.

---

## 4. Core Abstraction: Generic I/O Parameterization

The central technical challenge was threading an `IO: IoEngine` type parameter through ringwal's entire type hierarchy without breaking the existing API.

### 4.1 The IoEngine Trait Hierarchy

Three traits compose the I/O surface (defined in `ringwal::io`):

```rust
pub trait IoEngine: Send + Sync + Clone + 'static {
    type FileHandle: FileHandle;
    type ReadHandle: ReadHandle;

    fn open_append(&self, path: &Path, direct_io: bool) -> io::Result<Self::FileHandle>;
    fn open_read(&self, path: &Path) -> io::Result<Self::ReadHandle>;
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn write_file_bytes(&self, path: &Path, data: &[u8]) -> io::Result<()>;
    fn read_file_bytes(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn exists(&self, path: &Path) -> bool;
    fn now_secs(&self) -> u64;
}

pub trait FileHandle: Write + Send + 'static {
    fn sync_all(&mut self) -> io::Result<()>;
    fn sync_data(&mut self) -> io::Result<()>;
    fn try_clone_file(&self) -> io::Result<std::fs::File>;
    fn metadata_len(&self) -> io::Result<u64>;
}

pub trait ReadHandle: Read + Send + 'static {
    fn metadata_len(&self) -> io::Result<u64>;
}
```

The trait requires `Clone` because the engine instance is shared between the `Wal` handle, `SegmentManager`, flusher task, and checkpoint scheduler. For production `RealIo`, this is `Copy` (zero-size struct). For simulation `SimIo`, this is an `Arc<RefCell<SimFs>>` clone.

### 4.2 Generic Parameterization with Defaults

Every type that touches I/O becomes generic over `IO`, with `RealIo` as the default:

```rust
// Production types — generic with default
pub struct Wal<IO: IoEngine = RealIo> { ... }
pub struct Segment<IO: IoEngine> { ... }
pub struct SegmentManager<IO: IoEngine> { ... }

// Functions
pub fn recover<K, V, IO: IoEngine>(dir: &Path, io: &IO) -> Result<...> { ... }
pub fn checkpoint<K, V, IO: IoEngine>(dir: &Path, io: &IO) -> Result<...> { ... }
```

The default type parameter means existing code that uses `Wal` without specifying `IO` silently gets `Wal<RealIo>`, preserving full backward compatibility. No existing tests or user code needed to change.

### 4.3 Two Implementations

**`RealIo`** (production): a zero-size `#[derive(Clone, Copy)]` struct where every method is a thin wrapper around `std::fs`:

```rust
impl IoEngine for RealIo {
    type FileHandle = RealFileHandle;  // wraps BufWriter<File>
    type ReadHandle = RealReadHandle;  // wraps File

    fn open_append(&self, path: &Path, direct_io: bool) -> io::Result<RealFileHandle> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        // ...
    }
    fn now_secs(&self) -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
    }
}
```

**`SimIo`** (testing): an `Arc<UnsafeRefCell<SimFs>>` wrapping a `HashMap<PathBuf, SimFile>` filesystem, a seeded `SmallRng`, a `FaultConfig`, and a `SimClock`. Every method checks the fault config before performing an in-memory operation:

```rust
impl IoEngine for SimIo {
    type FileHandle = SimFileHandle;
    type ReadHandle = SimReadHandle;

    fn open_append(&self, path: &Path, _direct_io: bool) -> io::Result<SimFileHandle> {
        // Check parent dir exists, create or reopen SimFile, return handle
    }
    fn now_secs(&self) -> u64 {
        self.borrow().clock.now_secs()  // deterministic, controlled by test
    }
}
```

The `UnsafeRefCell` wrapper implements `Send + Sync` via `unsafe impl` — this is sound because DST is strictly single-threaded (`current_thread` tokio runtime), and the `Arc` exists only to satisfy `IoEngine: Clone`.

### 4.4 Timestamp Abstraction

One subtle change: `WalEntry::new_timestamp()` originally called `SystemTime::now()` directly. This was changed to accept a `u64` parameter, with callers passing `io.now_secs()`. This threads the clock through the I/O engine, so `SimClock` can control time deterministically and `RealIo` returns the real system clock. Timestamps are metadata — LSNs handle ordering (INV-WAL-01).

---

## 5. Implementation Stages

The implementation followed a six-phase plan. Phases 1–5 are complete; Phase 6 remains future work.

### Phase 1 — I/O Trait Hierarchy ✅

Created `ringwal::io::IoEngine`, `FileHandle`, and `ReadHandle` traits in `src/io/mod.rs`. Created `RealIo` in `src/io/real.rs` wrapping `std::fs` with zero overhead. This was a pure addition — no existing code changed, no tests broke.

### Phase 2 — Generic Parameterization ✅

Threaded `IO: IoEngine` through all types:

- `Segment<IO>` — replaced `BufWriter<File>` with `IO::FileHandle`
- `SegmentManager<IO>` — uses `IO` for directory operations and segment lifecycle
- `Wal<IO>` — owns `SegmentManager<IO>`, passes `IO` to flusher task
- `flusher_task<K,V,IO>`, `flush_and_notify<IO>` — generic flush pipeline
- `recover<K,V,IO>()`, `checkpoint<K,V,IO>()`, `truncate_segments_before<IO>()` — recovery functions
- `recover_into_store<K,V,IO>()` — store reconstruction

**Gate**: All 24 existing ringwal tests (8 unit + 16 integration) passed unchanged.

### Phase 3 — Simulated I/O Backend ✅

Created the `ringwal-sim` crate with five modules:

- **`sim_io.rs`** — `SimIo: IoEngine` with three-tier write model, `crash()`, fault injection
- **`fault.rs`** — `FaultConfig` with builder pattern and five configurable fault rates
- **`clock.rs`** — `SimClock` with `Cell<u64>`, `advance(secs)`, monotonicity guarantee
- **`oracle.rs`** — `CommitOracle<K,V>`, `verify_recovery()`, `VerificationResult`
- **`harness.rs`** — `RingwalSimulator` skeleton
- **`lib.rs`** — module declarations and re-exports

**Gate**: 27 unit tests covering SimIo write/read roundtrip, crash discards unsynced data, fault injection fires errors, oracle verification logic.

### Phase 4 — Simulator Harness ✅

Filled in `RingwalSimulator` with:

- `run_workload(steps)` — async workload driver with weighted random operations
- `crash_and_recover()` — `SimIo::crash()` then `recover::<String, Vec<u8>, SimIo>()`
- `assert_no_lost_commits()`, `assert_no_phantom_commits()` — panicking verification with diagnostic output (seed, fault config, crash count)
- `crash_recover_verify()` — convenience: crash + recover + verify in one call
- `WorkloadStats` — tracking commits, aborts, fault errors, writers registered, aborted tx_ids

**Gate**: 29 tests (27 Phase 3 + 2 new async harness tests).

### Phase 5 — Property Tests ✅

Created `tests/dst_tests.rs` with eight property tests, each iterating over 1000 seeds (configurable via `DST_SEEDS` env var):

| Test | Property | Invariant |
|------|----------|-----------|
| `dst_no_lost_commits` | Every oracle-committed transaction survives crash + recovery | INV-WAL-05 |
| `dst_no_phantom_commits` | No uncommitted data appears as committed after recovery | INV-WAL-07 |
| `dst_lsn_monotonicity` | No duplicate tx_ids; entries carry consistent tx_ids | INV-WAL-01 |
| `dst_checksum_integrity` | Recovery stats are consistent; all entries have valid tx_ids | INV-WAL-03 |
| `dst_segment_rotation_under_faults` | 4 KB segments + faults → correct recovery | INV-WAL-02, INV-WAL-04 |
| `dst_checkpoint_advancement` | Checkpoint + truncate → post-checkpoint commits survive | INV-WAL-05 |
| `dst_abort_discarded` | Aborted transaction IDs never appear as committed | INV-WAL-07 |
| `dst_multiple_crashes` | Two crash-recovery cycles → all commits survive | INV-WAL-05 |

Each test uses a `REGRESSION_SEEDS` array (initially empty) that runs known-interesting seeds before the random range. On failure, the test prints the exact seed and `FaultConfig` for replay.

**Gate**: 8 tests × 1000 seeds = 8000 scenarios, all passing.

### Phase 6 — Extended Fault Models ⬜ (future)

- **`TaskSpawner` trait** — abstract `tokio::spawn_blocking` and `std::thread::spawn` for Pipelined/Background sync mode DST
- **Delayed fsync model** — SimIo tracks "in-flight" syncs, commits after N simulator steps
- **Bitrot / corruption model** — random bit flips in durable data, testing CRC32 detection in recovery

---

## 6. The Bug DST Found

During Phase 5, the `dst_no_lost_commits` test failed at seed 15 with small segment sizes and 2% fsync fault rates. The failure revealed a **durability violation in the WAL's flush pipeline**.

### Root Cause

In `flush_and_notify()` (the non-pipelined flush path in `wal.rs`), the original code for `SyncMode::Full` was:

```rust
// BEFORE (buggy)
let _fsynced = segment_mgr.fsync().is_ok();
debug_assert_commit_durable!(_fsynced);

// Notify ALL commit waiters — even if fsync failed!
for waiter in commit_waiters.drain(..) {
    let _ = waiter.send(());
}
```

The fsync result was captured in `_fsynced` and checked only by a `debug_assert!` macro — which is stripped in release builds. When `fsync()` returned `Err` (simulated by fault injection), the commit waiters were still notified with `Ok(())`. From the writer's perspective, `tx.commit().await` returned `Ok` — the transaction appeared to be durably committed. But the data was still in the kernel buffer, not on "disk". A subsequent crash would discard it.

This is INV-WAL-05 in plain terms: **the WAL told the application the data was safe when it wasn't.**

### The Fix

```rust
// AFTER (fixed)
if let Err(e) = segment_mgr.fsync() {
    eprintln!("ringwal: fsync error: {e}");
    batch.clear();
    commit_waiters.clear();  // do NOT notify — commit is not durable
    return;
}
```

If `fsync()` fails, commit waiters are cleared and the function returns without sending any notification. The writer's `rx.await` will resolve to `Err(RecvError)` (the oneshot sender was dropped), which maps to `WalError::Closed`. The application sees the commit as failed and can retry. The same pattern was applied to `SyncMode::DataOnly` and `SyncMode::Background`.

### Why This Bug Matters

- **It was invisible to all existing tests.** Integration tests use real I/O where `fsync()` effectively never fails. No conventional test could reproduce it.
- **It was invisible in debug builds.** The `debug_assert!` would have caught it, but release builds — the ones actually deployed — silently dropped the error.
- **It's a real-world failure mode.** Disk full, NFS timeouts, EBS volume detachment, and cloud provider storage errors all cause `fsync()` failures. This bug would have caused silent data loss on any of those events.
- **DST found it in under 5 minutes of wall-clock time**, across 1000 seeds with just 2% fault rates. A production deployment might never encounter the specific sequence, or might encounter it once after months and lose data irrecover­ably.

---

## 7. Advantages of DST for ringwal

### 7.1 Exhaustive Edge Case Coverage

A single `dst_no_lost_commits` run with 1000 seeds explores 1000 distinct fault timelines: different numbers of writers, different transaction sizes, faults at different points in the flush pipeline, crashes at different moments. This is qualitatively different from hand-written test scenarios, which cover maybe 5–10 specific cases.

### 7.2 Reproducibility

Every failure prints its seed. Running with that seed reproduces the exact same fault sequence on any machine. This eliminates "works on my machine" and "can't reproduce" from the debugging workflow. It also enables regression seeds — a growing collection of known-interesting scenarios that are replayed on every CI run.

### 7.3 Black-Box Testing

Because `ringwal-sim` depends only on ringwal's public API, every test validates not just internal correctness but API sufficiency. If the simulator can't express a scenario (e.g., "checkpoint then write more"), that's a signal the public API has a gap.

### 7.4 Oracle-Based Verification > Assertion-Based Testing

Traditional tests assert specific outcomes: "after committing these 3 transactions, recovery should return them." Oracle-based testing asserts a universal property: "after any sequence of operations with any fault pattern, every acknowledged commit must survive recovery." This catches bugs that no specific test case was designed to find.

### 7.5 Speed

The in-memory SimIo eliminates all disk I/O latency. Running 8 property tests × 1000 seeds (8000 crash-recovery scenarios) completes in roughly 60 seconds on a standard laptop. The same coverage using real-disk integration tests would require orchestrating actual crashes (e.g., power-cut testing) and would take orders of magnitude longer.

### 7.6 Compositional Fault Models

Fault rates are independently tunable: 2% write failure + 2% fsync failure + 0% crash is a very different scenario from 0% write failure + 0% fsync failure + 1% crash. The builder pattern makes it easy to construct targeted fault scenarios or sweep across a parameter space.

### 7.7 Cost of the Abstraction

The `IO: IoEngine` generic parameter adds zero runtime cost for production code. `RealIo` is a zero-size type; every trait method is inlineable (and in practice does get monomorphised and inlined). The only cost is compile-time: one extra type parameter on core types and a second monomorphisation for `SimIo` in the test crate.

---

## 8. Test Inventory

| Scope | Count | Command |
|-------|-------|---------|
| SimIo unit tests | 12 | `cargo test -p ringwal-sim --release --lib` |
| SimClock unit tests | 4 | (included in `--lib`) |
| FaultConfig unit tests | 3 | (included in `--lib`) |
| Oracle unit tests | 5 | (included in `--lib`) |
| Harness unit tests | 5 | (included in `--lib`) |
| **Total unit tests** | **29** | `cargo test -p ringwal-sim --release --lib` |
| Property tests | 8 × 1000 seeds | `cargo test -p ringwal-sim --release --test dst_tests` |
| ringwal existing tests | 24 | `cargo test -p ringwal --release` |

---

## 9. Running the Simulator

```bash
# Quick smoke test (5 seeds)
DST_SEEDS=5 cargo test -p ringwal-sim --release --test dst_tests

# Full suite (1000 seeds, default)
cargo test -p ringwal-sim --release --test dst_tests

# Custom seed count
DST_SEEDS=5000 cargo test -p ringwal-sim --release --test dst_tests

# Single property test
cargo test -p ringwal-sim --release --test dst_tests dst_no_lost_commits

# Unit tests only
cargo test -p ringwal-sim --release --lib

# All tests (unit + property)
cargo test -p ringwal-sim --release

# Verify ringwal backward compatibility
cargo test -p ringwal --release
```

---

## 10. Future Directions

1. **Pipelined/Background sync mode DST** (Phase 6) — requires a `TaskSpawner` trait to intercept `spawn_blocking` calls and run them synchronously within the single-threaded runtime.

2. **Bitrot / corruption model** — random bit flips in the durable tier to validate CRC32 detection during recovery. Tests INV-WAL-03 more aggressively.

3. **Delayed fsync model** — instead of instant sync-to-durable, `sync_all()` moves data to an "in-flight" state that becomes durable after N simulator steps. Tests that the WAL correctly handles "fsync returned Ok but crash happened before the disk finished writing."

4. **Coverage-guided seed selection** — instrument the WAL with lightweight counters and preferentially select seeds that reach new code paths, similar to fuzzer feedback loops.

5. **Regression seed collection** — as production incidents or bug reports arrive, encode them as regression seeds in `REGRESSION_SEEDS` for permanent replay in CI.
