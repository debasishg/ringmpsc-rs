# Deterministic Simulation Testing (DST) for ringwal

> **Overview document**: This file provides a high-level introduction to the DST approach and implementation status. For the full architecture rationale, `IoEngine` trait design, `FaultConfig` parameters, `CommitOracle`, and the INV-WAL-05 bug discovery narrative, see [dst_arch.md](dst_arch.md).

**DST is now implemented in the `ringwal-sim` crate.** It gives *reproducible* confidence in the hardest parts: concurrent writers + background flusher + fsync ordering + recovery after crashes / partial writes / timing races.

The core idea: **run the entire system on a single thread, replace real I/O / time with controllable simulators, inject faults, and replay any failure with the exact same seed**. This turns flaky integration tests into deterministic property tests that can explore thousands of edge cases in seconds.

---

## 1. Core Principles

- **Single-threaded execution** -- everything runs in one async task via `#[tokio::test(flavor = "current_thread")]`.
- **Generic parameterization** -- all I/O abstracted behind an `IoEngine` trait. Production uses `RealIo`, DST uses `SimIo`. Same code paths, different backends.
- **Fault injection** at every realistic boundary: partial writes, fsync failures, segment rotation errors, crashes mid-flush.
- **Property assertions** instead of fixed scenarios (e.g. "after any sequence of appends + random faults, recovery must yield exactly the committed data").
- **Seed capture on failure** -- test prints the seed so you can replay *exactly* the failing run.
- **Oracle-based verification** -- simulator maintains ground truth of acknowledged commits, compared against recovery output.

---

## 2. Architecture

Ringwal is **not** a distributed system (no network), so we don't need `madsim` or `turmoil`. It is heavily I/O-bound, so the abstraction focuses on disk operations and time.

### Approach: Custom I/O Trait Hierarchy + Separate Sim Crate

- **`ringwal`** -- production crate. Gets an `IoEngine` trait hierarchy and `RealIo` implementation. All types become generic: `Wal<IO>`, `Segment<IO>`, `SegmentManager<IO>`, etc. Type aliases (`type DefaultWal = Wal<RealIo>`) preserve backward compatibility.
- **`ringwal-sim`** -- new crate (test/dev only). Contains `SimIo: IoEngine` (in-memory filesystem with 3-tier write model), `FaultConfig`, `SimClock`, `CommitOracle`, `RingwalSimulator`. Depends only on ringwal's **public API** (black-box testing).

This gives the strongest separation: zero simulation code in production, and validates that the public API is sufficient for external consumers.

### Why Not Other Approaches

| Rejected | Reason |
|---|---|
| `madsim` as primary runtime | Designed for network simulation; minimal file I/O support. Ringwal has zero networking. |
| `cfg(test)` / feature flags | Conditional compilation means DST may not test exact production code paths. |
| `SimDisk { write, fsync }` (narrow trait) | Actual I/O surface spans 15+ operations. Need full coverage. |
| Simulation code in `ringwal/src/` | Mixes production and test concerns. Separate crate is cleaner. |

### Retained for Later

- **`loom`** -- for exhaustive concurrency checking of the lock-free MPSC ring (already used in `ringmpsc`).
- **`TaskSpawner` trait** -- for DST of Pipelined/Background sync modes that use `spawn_blocking`/`std::thread::spawn`.

---

## 3. I/O Surface Analysis

The actual I/O surface that `IoEngine` must cover (from `segment.rs`, `recovery.rs`, `entry.rs`):

### File Write Operations (segment.rs)

| Operation | Current Call | Trait Method |
|---|---|---|
| Open file for append | `OpenOptions::new().create(true).append(true).open(path)` | `IoEngine::open_append(path) -> FileHandle` |
| Write data | `BufWriter::write_all(data)` | `FileHandle::write_all(data)` |
| Flush to kernel | `BufWriter::flush()` | `FileHandle::flush()` |
| Durable sync | `File::sync_all()` | `FileHandle::sync_all()` |
| Data-only sync | `File::sync_data()` | `FileHandle::sync_data()` |
| Clone fd (pipelined) | `File::try_clone()` | `FileHandle::try_clone()` |
| File size | `File::metadata()?.len()` | `FileHandle::metadata_len()` |
| Direct I/O | `fcntl(F_NOCACHE)` | `FileHandle::set_direct_io()` |

### File Read Operations (recovery.rs)

| Operation | Current Call | Trait Method |
|---|---|---|
| Open file for read | `File::open(path)` | `IoEngine::open_read(path) -> ReadHandle` |
| Read exact bytes | `file.read_exact(&mut buf)` | `ReadHandle::read_exact(&mut buf)` |
| File size | `file.metadata()?.len()` | `ReadHandle::metadata_len()` |

### Directory Operations (segment.rs, recovery.rs)

| Operation | Current Call | Trait Method |
|---|---|---|
| Create directory | `fs::create_dir_all(dir)` | `IoEngine::create_dir_all(dir)` |
| List entries | `fs::read_dir(dir)` | `IoEngine::read_dir(dir)` |
| Remove file | `fs::remove_file(path)` | `IoEngine::remove_file(path)` |
| Atomic write (checkpoint) | `fs::write(path, data)` | `IoEngine::write_file_bytes(path, data)` |
| Read all bytes (checkpoint) | `fs::read(path)` | `IoEngine::read_file_bytes(path)` |

### Time (entry.rs)

| Operation | Current Call | Trait Method |
|---|---|---|
| Current timestamp | `SystemTime::now().duration_since(UNIX_EPOCH).as_secs()` | `IoEngine::now_secs()` |

**Decision**: `WalEntry::new_timestamp()` changes to accept a `u64` parameter. Callers supply `io.now_secs()`. Timestamps are metadata; LSNs handle ordering (INV-WAL-01).

**All traits are synchronous** -- ringwal uses `std::fs` (blocking), not `tokio::fs`.

---

## 4. Crate Layout

```
crates/
  ringwal/                         -- production crate (modified)
    src/
      io/
        mod.rs                     -- IoEngine, FileHandle, ReadHandle traits
        real.rs                    -- RealIo: IoEngine (zero-overhead production backend)
      segment.rs                   -- Segment<IO>, SegmentManager<IO>
      wal.rs                       -- Wal<IO>, flusher_task<K,V,IO>
      recovery.rs                  -- recover<K,V,IO>(), checkpoint<IO>()
      entry.rs                     -- new_timestamp() accepts u64 parameter
      writer.rs                    -- WalWriterFactory<K,V,IO>, WalWriter<K,V,IO>
      store.rs                     -- recover_into_store<K,V,IO>()
      config.rs                    -- unchanged
      invariants.rs                -- unchanged
      error.rs                     -- unchanged
      transaction.rs               -- unchanged
      lib.rs                       -- adds io module, type aliases, re-exports

  ringwal-sim/                     -- NEW crate (test/dev only)
    Cargo.toml                     -- depends on ringwal, rand
    src/
      lib.rs                       -- re-exports all sim modules
      sim_io.rs                    -- SimIo: IoEngine (in-memory FS, 3-tier write model)
      fault.rs                     -- FaultConfig (failure probabilities, builder pattern)
      clock.rs                     -- SimClock (monotonic counter, advanceable)
      oracle.rs                    -- CommitOracle (ground truth tracker)
      harness.rs                   -- RingwalSimulator (workload driver)
    tests/
      dst_tests.rs                 -- all property-based DST tests + regression seeds
```

---

## 5. Implementation Plan

> **Status**: Phases 1–5 are **complete**. Phase 6 remains future work.

### Phase 1 -- I/O Trait Hierarchy (in `ringwal`) ✅

**Create `src/io/mod.rs`** -- core abstractions:

```rust
/// Top-level I/O abstraction. Production: RealIo. Simulation: SimIo.
pub trait IoEngine: Send + Sync + 'static {
    type FileHandle: FileHandle;
    type ReadHandle: ReadHandle;

    // File operations
    fn open_append(&self, path: &Path, direct_io: bool) -> io::Result<Self::FileHandle>;
    fn open_read(&self, path: &Path) -> io::Result<Self::ReadHandle>;

    // Directory operations
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;

    // Atomic file operations (checkpoint)
    fn write_file_bytes(&self, path: &Path, data: &[u8]) -> io::Result<()>;
    fn read_file_bytes(&self, path: &Path) -> io::Result<Vec<u8>>;

    // Clock
    fn now_secs(&self) -> u64;
}

pub trait FileHandle: Write + Send + 'static {
    fn sync_all(&mut self) -> io::Result<()>;
    fn sync_data(&mut self) -> io::Result<()>;
    fn try_clone(&self) -> io::Result<Self> where Self: Sized;
    fn metadata_len(&self) -> io::Result<u64>;
}

pub trait ReadHandle: Read + Send + 'static {
    fn metadata_len(&self) -> io::Result<u64>;
}
```

**Create `src/io/real.rs`** -- `RealIo` wrapping `std::fs`. Zero overhead.

### Phase 2 -- Generic Parameterization (in `ringwal`) ✅

Thread `IO: IoEngine` through all types:

1. `Segment<IO>` -- replace `BufWriter<File>` with `IO::FileHandle`
2. `SegmentManager<IO>` -- owns `Segment<IO>`, uses `IO` for dir ops
3. Recovery functions -- `recover<K,V,IO>()`, `read_segment_entries<K,V,IO>()`, `write_checkpoint<IO>()`, `read_checkpoint<IO>()`, `truncate_segments_before<IO>()`, `read_segment_max_tx_id<IO>()`
4. `Wal<IO>`, `flusher_task<K,V,IO>`, `flush_and_notify<IO>`
5. `WalWriterFactory<K,V,IO>`, `WalWriter<K,V,IO>`
6. `recover_into_store<K,V,IO>()`

**Type aliases for backward compatibility:**
```rust
pub type DefaultWal = Wal<RealIo>;
// ... equivalent aliases for other public types
```

**Gate**: `cargo test -p ringwal --release` -- all existing integration tests pass unchanged.

**DST scope**: `SyncMode::Full`, `DataOnly`, `None` only. Pipelined modes use `spawn_blocking` / `std::thread::spawn` which require a `TaskSpawner` trait (Phase 6).

### Phase 3 -- Simulated I/O Backend (in `ringwal-sim`) ✅

**`SimIo: IoEngine`** with in-memory filesystem:

- `HashMap<PathBuf, SimFile>` in `RefCell` (single-threaded DST -- no Mutex needed)
- **Three-tier per-file write model** (mirrors real OS behavior):
  - `write_buffer: Vec<u8>` -- unflushed application writes
  - `kernel_buffer: Vec<u8>` -- flushed but not synced
  - `durable: Vec<u8>` -- synced to "disk"
- `flush()` moves write_buffer -> kernel_buffer
- `sync_all()` / `sync_data()` moves kernel_buffer -> durable
- **`crash()`** -- discards write_buffer + kernel_buffer, keeps only durable. Key DST primitive.

**`FaultConfig`** -- controls failure probabilities:
```rust
pub struct FaultConfig {
    pub write_fail_rate: f64,       // 0.0-1.0
    pub fsync_fail_rate: f64,
    pub partial_write_rate: f64,
    pub crash_probability: f64,     // per-op chance of simulated crash
}
```

**`SimClock`** -- `Cell<u64>` monotonic counter with `advance(secs)`.

**Seeded RNG**: `SmallRng::seed_from_u64(seed)` drives all fault decisions. One master seed -> reproducible everything.

### Phase 4 -- Simulator Harness (in `ringwal-sim`) ✅

**`RingwalSimulator`**:
```rust
struct RingwalSimulator {
    sim_io: Arc<SimIo>,
    oracle: CommitOracle,
    rng: SmallRng,
    fault_config: FaultConfig,
}
```

- `new(seed, fault_config, wal_config)` -- initialize SimIo + seeded RNG
- `run_workload(steps)` -- random ops: create writers, append entries, commit/abort, advance clock. Records acknowledged commits in oracle.
- `crash_and_recover()` -- `SimIo::crash()` then `recover::<String, Vec<u8>, SimIo>(...)`
- `assert_properties(recovered)` -- delegates to oracle + invariant checks

**`CommitOracle`** -- ground truth:
- `record_commit(tx_id, entries)` -- called when WAL commit returns Ok
- `verify_against(recovered)` -- asserts oracle is a subset of recovered committed set

### Phase 5 -- Property Tests (in `ringwal-sim/tests/`) ✅

Using `#[tokio::test(flavor = "current_thread")]` for deterministic task scheduling:

| Test | Property | Spec Reference |
|---|---|---|
| `dst_no_lost_commits` | All oracle-committed txns are recovered | INV-WAL-05 |
| `dst_no_phantom_commits` | No uncommitted data appears as committed | INV-WAL-07 |
| `dst_lsn_monotonicity` | Recovered LSNs strictly increasing | INV-WAL-01 |
| `dst_checksum_integrity` | Every recovered entry has valid CRC32 | INV-WAL-03 |
| `dst_segment_rotation_under_faults` | Small `max_segment_size` + faults -> correct | INV-WAL-02, INV-WAL-04 |
| `dst_checkpoint_advancement` | Checkpoint + truncate + recover -> no loss | INV-WAL-05 |
| `dst_abort_discarded` | Aborted txns never in recovered set | INV-WAL-07 |
| `dst_multiple_crashes` | Crash -> partial recovery -> write -> crash -> verify | INV-WAL-05 |

Each test loops over `0..NUM_SEEDS` (configurable via `DST_SEEDS` env var, default 1000). On failure: prints seed + `FaultConfig` for exact replay.

**Regression seeds**: hardcoded `Vec<u64>` of known-interesting seeds (initially empty, populated as bugs are found).

### Phase 6 -- Extended Fault Models (future, out of scope) ⬜

- **`TaskSpawner` trait** -- abstract `tokio::spawn_blocking` and `std::thread::spawn` for Pipelined/Background sync mode DST. Simulated spawner runs closures synchronously.
- **Delayed fsync model** -- SimIo tracks "in-flight" syncs, durably commits after N simulator steps.
- **Bitrot / corruption model** -- random bit flips in durable data, tests CRC32 detection in recovery.

---

## 6. Bugs Found by DST

### INV-WAL-05: fsync failure silently acknowledged commits

**Found during**: Phase 5 property tests (`dst_no_lost_commits`, seed 15).

**Root cause**: In `flush_and_notify()` (`wal.rs`), when `segment_mgr.fsync()` returned `Err`, the result was stored in a variable checked only by `debug_assert!` (inactive in release builds). Commit waiters were still notified with `Ok(())`, violating INV-WAL-05 — a committed transaction must be durable before acknowledgment.

**Fix**: If `fsync()` / `fsync_data()` fails, clear `commit_waiters` and return without notifying — matching the existing write-failure behaviour. Affected code paths: `SyncMode::Full`, `DataOnly`, `Background`.

**Regression seed**: `REGRESSION_SEEDS` in `dst_tests.rs` (currently empty; seed 15 with `small_segment_config` + 2% fault rates reproduces the original issue on the pre-fix code).

---

## 7. Running the Simulator

> **Threading note**: The single-threaded constraint applies **within** each test
> (`#[tokio::test(flavor = "current_thread")]` gives deterministic async scheduling).
> The test *harness* can run multiple test functions in parallel — each test creates
> its own independent `RingwalSimulator` with no shared state.

### Quick smoke test (5 seeds, fast)

```bash
DST_SEEDS=5 cargo test -p ringwal-sim --release --test dst_tests
```

### Full property test suite (1000 seeds, default)

```bash
cargo test -p ringwal-sim --release --test dst_tests
```

### Custom seed count

```bash
DST_SEEDS=5000 cargo test -p ringwal-sim --release --test dst_tests
```

### Run a single property test

```bash
cargo test -p ringwal-sim --release --test dst_tests dst_no_lost_commits
cargo test -p ringwal-sim --release --test dst_tests dst_segment_rotation_under_faults
```

### Limit test parallelism (if memory-constrained)

Each test iterates 1000 seeds sequentially, so 8 tests running in parallel
is fine on most machines. To restrict:

```bash
cargo test -p ringwal-sim --release --test dst_tests -- --test-threads=4
cargo test -p ringwal-sim --release --test dst_tests -- --test-threads=1  # sequential
```

### Unit tests only (SimIo, FaultConfig, SimClock, Oracle, Harness)

```bash
cargo test -p ringwal-sim --release --lib
```

### All ringwal-sim tests (unit + property)

```bash
cargo test -p ringwal-sim --release
```

### Verify ringwal backward compatibility

```bash
cargo test -p ringwal --release
```

### Replay a failing seed

On failure, the test prints the seed and `FaultConfig`. To replay:

```bash
# Run only the failing test with a single seed
DST_SEEDS=1 cargo test -p ringwal-sim --release --test dst_tests <test_name>
```

For exact replay of a specific seed, add it to the `REGRESSION_SEEDS` array in `tests/dst_tests.rs` (regression seeds run before the random range).

---

## 8. Verification Gates

| Gate | Command | Validates |
|---|---|---|
| Phase 2 | `cargo test -p ringwal --release` | All existing tests pass (backward compat) |
| Phase 3 | `cargo test -p ringwal-sim --release --lib` | SimIo unit tests (write/read roundtrip, crash discards unsynced, faults trigger errors) |
| Phase 5 | `cargo test -p ringwal-sim --release` | 1000+ seeds pass across all property tests |
| API compat | `cargo doc -p ringwal --no-deps` | Public API unchanged |
| CI | `cargo test -p ringwal-sim --release` | Added to CI pipeline |

---

## 9. Decisions

| Decision | Rationale |
|---|---|
| **Separate `ringwal-sim` crate** | Zero simulation code in production; validates public API sufficiency; fits workspace pattern |
| **Generic parameterization over feature flags** | Same code paths in prod and test; `IO = RealIo` default for zero API breakage |
| **Synchronous traits** | Ringwal uses `std::fs` (blocking); SimIo mirrors this |
| **No madsim dependency** | Custom SimIo is simpler, targeted for non-distributed system |
| **`RefCell`-based SimIo** | DST is single-threaded by design; no Mutex overhead |
| **DST scoped to Full/DataOnly/None** | Pipelined modes deferred to Phase 6 (`TaskSpawner` trait) |
| **`#[tokio::test(flavor = "current_thread")]`** | Deterministic task scheduling without abstracting `tokio::spawn` |
| **`new_timestamp()` accepts `u64`** | Minimal change; timestamps are metadata, LSNs handle ordering |
| **Oracle-based verification** | Ground truth vs recovery output; strongest correctness guarantee |
| **`ringwal-sim` depends only on public API** | Black-box testing; no `pub(crate)` access needed |
