# ringwal — Architecture

## Overview

`ringwal` is a Write-Ahead Log that uses **per-writer SPSC ring buffers** (via `ringmpsc-rs`)
instead of a single shared queue. This document describes the architecture, data flow,
and design decisions.

## Design Motivation

A traditional shared-queue WAL uses a single concurrent queue (e.g. `crossbeam::SegQueue`)
shared by all writers:

```
Writer A ──push──┐
Writer B ──push──┤── SegQueue ──pop──▶ Flusher ──▶ WAL file
Writer C ──push──┘      (CAS)
```

This works well at low writer counts, but CAS contention grows linearly with concurrent writers.
Each `push` performs a compare-and-swap on the queue's tail pointer, and under high contention
multiple retries are needed.

`ringwal` eliminates this by giving each writer a **dedicated SPSC ring buffer**:

```
Writer A ──▶ Ring A ──┐
Writer B ──▶ Ring B ──┤──▶ Flusher ──▶ Segment files
Writer C ──▶ Ring C ──┘
                  (sequential poll)
```

Each writer writes to its own ring with no contention whatsoever. The single flusher
task sequentially polls all rings — this is the only point of aggregation, and it runs
on a single thread with no synchronization.

## Component Architecture

```
┌─────────────────────────────────────────────────────────┐
│                        User Code                        │
│                                                         │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  │
│  │ Transaction  │  │ Transaction  │  │ Transaction  │  │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘  │
│         │                 │                 │           │
│  ┌──────▼───────┐  ┌──────▼───────┐  ┌──────▼───────┐  │
│  │  WalWriter   │  │  WalWriter   │  │  WalWriter   │  │
│  │ (RingSender) │  │ (RingSender) │  │ (RingSender) │  │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘  │
└─────────┼──────────────────┼──────────────────┼─────────┘
          │ SPSC Ring A      │ SPSC Ring B      │ SPSC Ring C
          │                  │                  │
┌─────────▼──────────────────▼──────────────────▼─────────┐
│                 RingReceiver (Stream)                    │
│             polls all rings sequentially                 │
├─────────────────────────────────────────────────────────┤
│                    Flusher Task                          │
│  1. Drain envelopes into batch                          │
│  2. Serialize + CRC32 + write to SegmentManager         │
│  3. fsync active segment (7 sync modes; see below)      │
│  4. Notify commit waiters (group commit)                │
├─────────────────────────────────────────────────────────┤
│                  SegmentManager                          │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐                │
│  │wal-01.log│ │wal-02.log│ │wal-03.log│ ← rotatable    │
│  │ (sealed) │ │ (sealed) │ │ (active) │                 │
│  └──────────┘ └──────────┘ └──────────┘                 │
└─────────────────────────────────────────────────────────┘
```

### Key Types

| Type | Role |
|------|------|
| `Wal` | Engine handle — owns flusher task, exposes `shutdown()` |
| `WalWriterFactory<K, V>` | Registers new writers (allocates SPSC ring per writer) |
| `WalWriter<K, V>` | Per-writer handle wrapping `RingSender<Envelope<K, V>>` |
| `Transaction<K, V>` | Buffers ops locally, flushes atomically on `commit()` |
| `Envelope<K, V>` | Internal: `Entry(WalEntry)` or `CommitBarrier { entry, tx }` |
| `SegmentManager` | Manages active + sealed segment files, rotation, truncation |
| `WalEntry<K, V>` | On-disk entry: Insert / Update / Delete / Commit / Abort |
| `WalEntryHeader` | 13-byte header: length (u64) + CRC32 (u32) + version (u8) |
| `WalConfig` | Configuration: dir, ring capacity, max writers, segment size, sync mode, etc. |
| `SyncMode` | Durability mode enum (7 variants — see Sync Modes below) |
| `RecoveryStats` | Recovery metrics: total, committed, aborted, incomplete, partial writes, checksum failures |
| `SegmentMeta` | Per-segment metadata: id, path, size, entry count, first/last LSN |
| `TxState` | Transaction lifecycle: Active / Committed / Aborted |
| `WalError` | Error enum: ChecksumMismatch, SegmentFull, NoNewCheckpoints, etc. |
| `WalStore<K, V>` | Storage backend trait — lives in `ringwal-store` crate |
| `InMemoryStore<K, V>` | Thread-safe HashMap store — lives in `ringwal-store` crate |

## Data Flow

### Write Path

1. **Transaction buffers locally** — `tx.insert(k, v)` appends to an in-memory `Vec<WalEntry>`.
   No I/O or ring interaction.

2. **Commit flushes to ring** — `tx.commit(&writer)` sends each buffered entry as
   `Envelope::Entry(entry)` through the writer's SPSC ring, then sends a
   `Envelope::CommitBarrier { entry: Commit, tx: oneshot::Sender }`.

3. **Flusher drains batch** — The background flusher task uses `tokio::select!` to wait
   on either data from `RingReceiver::next()` or a shutdown signal. Once the first item
   arrives, it opportunistically drains up to `batch_hint` more items (with a 100μs timeout).

4. **Serialize + write** — Each entry is serialized via `bincode`, CRC32-checksummed,
   wrapped in a 13-byte header, and written to the active segment file via `BufWriter`.

5. **Segment rotation** — If the active segment exceeds `max_segment_size`, the
   `SegmentManager` seals it and opens the next segment.

6. **fsync + notify** — The active segment is fsynced, then all `oneshot::Sender`s
   collected during the batch are fired. Each `commit()` call awaiting its oneshot
   now returns — this is the **group commit** mechanism.

   The fsync behaviour depends on the configured `SyncMode` (see Sync Modes below).
   In pipelined modes the fsync is fire-and-forget and commit waiters are notified
   from the fsync thread after *their* batch completes, while the flusher immediately
   drains the next batch — overlapping I/O with data collection.
   See [`PIPELINED_FSYNC.md`](./PIPELINED_FSYNC.md) and
   [`PIPELINED_IMPROVEMENTS.md`](./PIPELINED_IMPROVEMENTS.md) for details.

### Sync Modes

`SyncMode` controls when and how the flusher syncs data to disk after writing a batch.
All modes preserve [INV-WAL-05](#invariants) (commit durability).

| Variant | fsync call | Blocking model | Notes |
|---------|-----------|----------------|-------|
| `Full` (default) | `sync_all()` | Inline — blocks flusher | Maximum durability; simplest model |
| `DataOnly` | `sync_data()` | Inline — blocks flusher | Skips metadata sync; faster on Linux/ext4, equivalent to `Full` on macOS |
| `Background` | `sync_all()` | `spawn_blocking`, awaited | Yields flusher thread for writers; requires `multi_thread` runtime |
| `Pipelined` | `sync_all()` | `spawn_blocking`, fire-and-forget | Overlaps batch N+1 write with batch N fsync; requires `multi_thread` |
| `PipelinedDataOnly` | `sync_data()` | `spawn_blocking`, fire-and-forget | `Pipelined` + fdatasync — potentially 30–80% faster on Linux; equivalent to `Pipelined` on macOS |
| `PipelinedDedicated` | `sync_all()` | Dedicated OS thread + bounded channel | Lower tail latency at high writer counts; requires `multi_thread` |
| `None` | — (flush only) | None | Flushes BufWriter to kernel buffer only — no fsync; data may be lost on crash. For benchmarks/testing. |

### Read Path (Recovery)

1. **Discover segments** — `recover()` scans the WAL directory for files matching
   `wal-{id}.log`, sorted by segment ID.

2. **Sequential scan** — Each segment is read entry-by-entry: header → validate CRC32
   → deserialize → classify by `tx_id`.

3. **Transaction classification** — Entries are grouped by `tx_id`:
   - Has `Commit` marker → `RecoveryAction::Commit`
   - Has `Abort` marker → `RecoveryAction::Rollback`
   - Neither → `RecoveryAction::Incomplete`

4. **Return structured data** — `recover()` returns `Vec<RecoveredTransaction<K, V>>`
   and `RecoveryStats`. The caller decides what to replay.

## Design Comparison: Shared Queue vs Ring Decomposition

### Architecture Differences

| Aspect | Shared-queue WAL | ringwal |
|--------|--------------|---------|
| Writer channels | Single shared `SegQueue` | Dedicated SPSC ring per writer |
| Contention model | CAS on shared tail pointer | Zero writer-writer contention |
| Commit guarantee | Per-transaction `flush()` | Group commit (one fsync per batch) |
| File layout | Single monolithic WAL file | Multiple rotatable segment files |
| K/V types | Fixed `String/Vec<u8>` | Generic `K: Serialize, V: Serialize` |
| Storage engine | Built-in HashMap + LMDB | WAL-only — bring your own store |
| Cleanup | None (single file) | `truncate_before(lsn)` removes sealed segments |

### Concurrency Model

**Shared-queue WAL:**
- Writers: `SegQueue::push()` — lock-free CAS, contention ∝ writer count
- Flusher: `Mutex<BufWriter>` guarded, 10ms poll interval
- Backpressure: `Notify` when queue full, checked with `Acquire`/`Release`
- Commit: writer calls `wal.flush()` directly, which acquires `Mutex` → sequential fsyncs

**ringwal:**
- Writers: `RingSender::send()` — lock-free SPSC, zero other-writer contention
- Flusher: single task draining `RingReceiver` (polls all rings), no mutex
- Backpressure: built into `ringmpsc-stream` — sender awaits `backpressure_notify`
- Commit: writer awaits `oneshot::Receiver` → group commit on fsync
- SyncModes (7 variants):
  - `Full` — inline `sync_all()`, blocks flusher (default)
  - `DataOnly` — inline `sync_data()` (fdatasync), skips metadata
  - `Background` — `sync_all()` via `spawn_blocking`, awaited
  - `Pipelined` — fire-and-forget `sync_all()` via `spawn_blocking`, overlaps next batch
  - `PipelinedDataOnly` — `Pipelined` with `sync_data()`, 30–80% faster on Linux
  - `PipelinedDedicated` — dedicated OS thread + bounded channel for fsync, lower tail latency
  - `None` — flush BufWriter to kernel only (no fsync, for benchmarks/testing)

### Entry Format

ringwal uses a standard 13-byte header:

```
Offset  Size  Field
0       8     length: u64 LE   (payload size in bytes)
8       4     checksum: u32 LE (CRC32 of payload)
12      1     version: u8      (currently 1)
13      N     payload          (bincode-serialized WalEntry)
```

### Recovery Protocol

| Step | Shared-queue WAL | ringwal |
|------|--------------|---------|
| File discovery | Single `wal.log` | Multiple `wal-{id}.log` segments |
| Scan order | Sequential from offset 0 | Per-segment, segments in ascending ID order |
| Corruption handling | Stop entire recovery at first error | Stop within segment, continue to next |
| Output | Replays to HashMap store | Returns `Vec<RecoveredTransaction>` |
| Checkpoint | `wal.log.checkpoint` with tx_id | `checkpoint` file with LSN (u64 LE) |
| Cleanup after checkpoint | None | `truncate_before(lsn)` deletes old segments |

## Feature Matrix

### Core Features

| Feature | Status | Notes |
|---------|:---:|-------|
| Core WAL engine | ✅ | Per-writer SPSC rings with background flusher |
| Lock-free writes | ✅ | SPSC rings — zero writer-writer contention |
| Async/await support | ✅ | tokio-based |
| CRC32 checksums | ✅ | 13-byte header |
| Multi-writer support | ✅ | Dedicated ring per writer |
| Transaction abstraction | ✅ | `TxState` enum |
| Commit / Abort markers | ✅ | `WalEntry` variants |
| Graceful shutdown | ✅ | Drains in-flight entries |
| Crash recovery | ✅ | Multi-segment scan with CRC32 validation |
| Recovery statistics | ✅ | committed, aborted, incomplete, partial_writes, checksum_failures |
| Checkpoint read/write | ✅ | LSN-based |
| Partial write detection | ✅ | Handles truncated headers |
| Backpressure | ✅ | Ring-based via `ringmpsc-stream` |
| Error types | ✅ | ChecksumMismatch, SegmentFull, NoNewCheckpoints, etc. |

### Advanced Features

| Feature | Status | Description |
|---------|:---:|-------------|
| Per-writer SPSC rings | ✅ | Zero writer-writer contention |
| Group commit | ✅ | One fsync unblocks all commits in batch |
| Segment rotation | ✅ | Configurable max segment size |
| Segment truncation | ✅ | `truncate_before(lsn)` removes old segments |
| Generic K/V types | ✅ | Any `Serialize + DeserializeOwned` types |
| Configurable max writers | ✅ | Enforced at registration time |
| Configurable batch hint | ✅ | Flusher aggregation tuning |
| Per-ring metrics | ✅ | Optional via ringmpsc-rs |
| LSN-stamped entries | ✅ | Monotonic log sequence numbers |

### Not Yet Implemented — Ownership & Dependencies

> **As of 2026-03-27**: All WAL-internal items below are complete. The only remaining open
> work is the LMDB storage backend, which lives outside `ringwal` proper.

The remaining TODO items split into two categories: **WAL-internal** (belongs in ringwal)
and **client-side** (belongs in consumer crates or user code).

#### WAL-Internal (belongs in ringwal)

| Feature | Status | Description |
|---------|:---:|-------------|
| ~~Checkpoint advancement logic~~ | ✅ | `Wal::checkpoint()` scans recovery state, filters committed txns, advances to highest committed LSN, returns `NoNewCheckpoints` when idle. |
| ~~Automatic checkpoint scheduler~~ | ✅ | `Wal::start_checkpoint_scheduler(interval)` — periodic background task calling `checkpoint()` + `truncate_before()`. |
| ~~Benchmarks~~ | ✅ | Throughput benchmarks in `crates/ringwal/benches/wal_throughput.rs` at 1/2/4/8 writer counts via criterion. ringwal scales linearly with writers. |
| ~~CLI demo tool~~ | ✅ | `bin/demo.rs` (minimal CLI) and `examples/demo.rs` (full lifecycle) showing multi-writer transactions, recovery into `InMemoryStore`, and checkpoint scheduling. |

#### Client-Side (lives in `ringwal-store` crate)

| Feature | Status |
|---------|:---:|
| `WalStore<K,V>` trait | ✅ — `ringwal-store` crate |
| `InMemoryStore<K,V>` | ✅ — `ringwal-store` crate |
| `recover_into_store()` | ✅ — `ringwal-store` crate |
| `apply_transactions()` | ✅ — `ringwal-store` crate |
| LMDB storage backend | A persistent storage engine that *uses* the WAL. Should be a separate crate (e.g., `ringwal-lmdb`) depending on `ringwal-store`. |

#### Dependency Graph

```
                    ┌───────────────────────┐
                    │  Checkpoint advance-   │
                    │  ment logic       ✅   │
                    └──────────┬────────────┘
                               │ depends on
                    ┌──────────▼────────────┐
                    │  Automatic checkpoint  │
                    │  scheduler        ✅   │
                    └──────────┬────────────┘
                               │ uses
                    ┌──────────▼────────────┐
                    │  truncate_before()     │
                    │  (already done ✅)     │
                    └───────────────────────┘

┌─────────────┐     ┌──────────────────────┐
│ WalStore    │◀────│  Apply-to-store on   │
│ trait    ✅  │     │  recovery         ✅  │
│ (ringwal)   │     │  recover_into_store()│
└──────┬──────┘     └──────────────────────┘
       │ implemented by
       ├──────────────────┐
       │                  │
┌──────▼──────┐    ┌──────▼──────┐
│ InMemory    │    │ LMDB store  │
│ Store    ✅  │    │ (ringwal-   │
│ (ringwal-   │    │  lmdb)      │
│  store)     │    └─────────────┘
└─────────────┘

┌──────────────┐    ┌──────────────┐
│ Benchmarks ✅ │    │ CLI demo  ✅  │
│ (criterion)  │    │ (bin+example)│
└──────────────┘    └──────────────┘
```

#### Recommended Implementation Order

1. ~~**Checkpoint advancement logic**~~ — ✅ done, no dependencies
2. ~~**Automatic checkpoint scheduler**~~ — ✅ done, depends on #1
3. ~~**`WalStore` trait**~~ — ✅ done, `src/store.rs`
4. ~~**In-memory HashMap store**~~ — ✅ done, `InMemoryStore<K,V>` ships with ringwal
5. ~~**Apply-to-store on recovery**~~ — ✅ done, `recover_into_store()` + `apply_transactions()`
6. ~~**Benchmarks**~~ — ✅ done, `benches/wal_throughput.rs` (criterion, ringwal throughput at multiple writer counts)
7. **LMDB storage backend** (separate crate) — depends on #3, lowest priority
8. ~~**CLI demo**~~ — ✅ done, `bin/demo.rs` + `examples/demo.rs`

Only #7 (LMDB) remains.

## Invariants

Formal invariants are documented in [`spec.md`](../spec.md) and enforced via
`debug_assert!` macros in `src/invariants.rs`:

| ID | Invariant | Enforcement |
|----|-----------|-------------|
| `INV-WAL-01` | **LSN Monotonicity** — LSNs are strictly increasing within and across segments | `debug_assert_lsn_monotonic!` |
| `INV-WAL-02` | **Segment Size Bound** — segment file size ≤ `max_segment_size` (+ at most one entry) | `debug_assert_segment_size!` |
| `INV-WAL-03` | **Entry Integrity** — every persisted entry has a valid CRC32 checksum | `debug_assert_entry_checksum!` |
| `INV-WAL-04` | **Segment ID Monotonicity** — segment IDs are strictly increasing | `debug_assert_segment_id_monotonic!` |
| `INV-WAL-05` | **Commit Durability** — commit waiter notified only after its batch is fsynced (all sync modes) | `debug_assert_commit_durable!` |
| `INV-WAL-06` | **Per-Writer SPSC** — each writer is sole producer of its ring | Structural (design) |
| `INV-WAL-07` | **Transaction Atomicity** — a transaction's entries are all-or-nothing; `Commit` marker is the linearization point | Structural (design) |

## Configuration

The `WalConfig` struct controls all runtime parameters:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `dir` | `PathBuf` | (required) | Directory for WAL segment files |
| `ring_bits` | `u8` | 14 (16,384 slots) | Power-of-2 ring capacity per writer |
| `max_writers` | `usize` | 16 | Maximum concurrent writers |
| `max_segment_size` | `u64` | 64 MB | Max segment file size before rotation |
| `flush_interval` | `Duration` | 10 ms | Background flusher poll interval |
| `batch_hint` | `usize` | 256 | Hint for batch drain size per flush cycle |
| `enable_metrics` | `bool` | false | Enable per-ring metrics collection |
| `sync_mode` | `SyncMode` | `Full` | Durability mode (7 variants — see Sync Modes) |

Builder methods: `with_ring_bits()`, `with_max_writers()`, `with_max_segment_size()`,
`with_flush_interval()`, `with_batch_hint()`, `with_metrics()`, `with_sync_mode()`.

## Crate Dependencies

```
ringwal
├── ringmpsc-rs          (core SPSC ring buffers)
├── ringmpsc-stream      (async Stream/Sink adapters, SenderFactory)
├── tokio                (async runtime: sync, time, rt, fs, io-util)
├── serde + bincode      (entry serialization)
├── crc32fast            (CRC32 checksums)
└── thiserror            (error derive)

ringwal-store
├── ringwal              (WAL engine)
└── serde                (DeserializeOwned bounds)
```

## Benchmarks

`benches/wal_throughput.rs` contains 8 criterion benchmark groups:

| Group | Description |
|-------|-------------|
| `wal_durable` | `SyncMode::Full` — 1/2/4/8 writers |
| `wal_durable_bg` | `SyncMode::Background` — 1/2/4/8 writers |
| `wal_streaming_pipeline` | All 7 sync modes compared side-by-side |
| `wal_durable_tuning` | `flush_interval` × `batch_hint` parameter sweep |
| `wal_pipelined_tuning` | Pipelined-specific tuning sweep |
| `wal_throughput` | `SyncMode::None` on `current_thread` runtime |
| `wal_throughput_mt` | `SyncMode::None` on `multi_thread` runtime |
| `wal_streaming_payload` | Large payloads (64B / 1KB / 4KB) × writer counts |

## Future Directions

1. **LMDB storage backend** — A persistent storage engine using the WAL, as a
   separate crate (e.g., `ringwal-lmdb`) implementing the `WalStore` trait.
