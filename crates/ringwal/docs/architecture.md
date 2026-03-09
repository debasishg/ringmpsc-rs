# ringwal — Architecture

## Overview

`ringwal` is a Write-Ahead Log that uses **per-writer SPSC ring buffers** (via `ringmpsc-rs`)
instead of a single shared queue. This document describes the architecture, data flow,
and design decisions — compared against the reference implementation
[`async-wal-db`](https://github.com/debasishg/async-wal-db).

## Design Motivation

`async-wal-db` uses a single `crossbeam::SegQueue` shared by all writers:

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
│  3. fsync active segment (Full/Background/Pipelined)    │
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

   In **Pipelined** mode, the fsync is fire-and-forget via `spawn_blocking` and commit
   waiters are notified directly from the blocking thread after *their* batch's fsync
   completes. The flusher immediately returns to drain the next batch, overlapping
   I/O with data collection. See [`PIPELINED_FSYNC.md`](./PIPELINED_FSYNC.md) for details.

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

## Comparison with async-wal-db

### Architecture Differences

| Aspect | async-wal-db | ringwal |
|--------|--------------|---------|
| Writer channels | Single shared `SegQueue` | Dedicated SPSC ring per writer |
| Contention model | CAS on shared tail pointer | Zero writer-writer contention |
| Commit guarantee | Per-transaction `flush()` | Group commit (one fsync per batch) |
| File layout | Single monolithic WAL file | Multiple rotatable segment files |
| K/V types | Fixed `String/Vec<u8>` | Generic `K: Serialize, V: Serialize` |
| Storage engine | Built-in HashMap + LMDB | WAL-only — bring your own store |
| Cleanup | None (single file) | `truncate_before(lsn)` removes sealed segments |

### Concurrency Model

**async-wal-db:**
- Writers: `SegQueue::push()` — lock-free CAS, contention ∝ writer count
- Flusher: `Mutex<BufWriter>` guarded, 10ms poll interval
- Backpressure: `Notify` when `queue_size >= max_queue_size`, checked with `Acquire`/`Release`
- Commit: writer calls `wal.flush()` directly, which acquires `Mutex` → sequential fsyncs

**ringwal:**
- Writers: `RingSender::send()` — lock-free SPSC, zero other-writer contention
- Flusher: single task draining `RingReceiver` (polls all rings), no mutex
- Backpressure: built into `ringmpsc-stream` — sender awaits `backpressure_notify`
- Commit: writer awaits `oneshot::Receiver` → group commit on fsync
- SyncModes: `Full` (inline fsync), `Background` (awaited `spawn_blocking`),
  `Pipelined` (fire-and-forget `spawn_blocking` — overlaps next batch drain with
  previous fsync, 29% faster at 32 writers)

### Entry Format (Identical)

Both use the same 13-byte header:

```
Offset  Size  Field
0       8     length: u64 LE   (payload size in bytes)
8       4     checksum: u32 LE (CRC32 of payload)
12      1     version: u8      (currently 1)
13      N     payload          (bincode-serialized WalEntry)
```

### Recovery Protocol

| Step | async-wal-db | ringwal |
|------|--------------|---------|
| File discovery | Single `wal.log` | Multiple `wal-{id}.log` segments |
| Scan order | Sequential from offset 0 | Per-segment, segments in ascending ID order |
| Corruption handling | Stop entire recovery at first error | Stop within segment, continue to next |
| Output | Replays to HashMap store | Returns `Vec<RecoveredTransaction>` |
| Checkpoint | `wal.log.checkpoint` with tx_id | `checkpoint` file with LSN (u64 LE) |
| Cleanup after checkpoint | None | `truncate_before(lsn)` deletes old segments |

## Feature Compliance Matrix

### Completed (parity with async-wal-db)

| Feature | async-wal-db | ringwal | Notes |
|---------|:---:|:---:|-------|
| Core WAL engine | ✅ | ✅ | Different concurrency models |
| Lock-free writes | ✅ | ✅ | SegQueue vs SPSC rings |
| Async/await support | ✅ | ✅ | Both tokio-based |
| CRC32 checksums | ✅ | ✅ | Identical 13-byte header |
| Multi-writer support | ✅ | ✅ | Shared queue vs dedicated rings |
| Transaction abstraction | ✅ | ✅ | Matching `TxState` enum |
| Commit / Abort markers | ✅ | ✅ | Same `WalEntry` variants |
| Graceful shutdown | ✅ | ✅ | Both drain in-flight entries |
| Crash recovery | ✅ | ✅ | Different file layouts, same logic |
| Recovery statistics | ✅ | ✅ | Same fields |
| Checkpoint read/write | ✅ | ✅ | Different file conventions |
| Partial write detection | ✅ | ✅ | Both handle truncated headers |
| Backpressure | ✅ | ✅ | Notify-based vs ring-based |
| Error types | ✅ | ✅ | Overlapping error variants |

### Features Beyond async-wal-db (ringwal extras)

| Feature | Status | Description |
|---------|:---:|-------------|
| Per-writer SPSC rings | ✅ | Zero writer-writer contention |
| Group commit | ✅ | One fsync unblocks all commits in batch |
| Segment rotation | ✅ | Configurable max segment size |
| Segment truncation | ✅ | `truncate_before(lsn)` removes old segments |
| Generic K/V types | ✅ | Not limited to `String/Vec<u8>` |
| Configurable max writers | ✅ | Enforced at registration time |
| Configurable batch hint | ✅ | Flusher aggregation tuning |
| Per-ring metrics | ✅ | Optional via ringmpsc-rs |
| LSN-stamped entries | ✅ | Monotonic log sequence numbers |

### Not Yet Implemented — Ownership & Dependencies

The remaining TODO items split into two categories: **WAL-internal** (belongs in ringwal)
and **client-side** (belongs in consumer crates or user code).

#### WAL-Internal (belongs in ringwal)

| Feature | Status | Description |
|---------|:---:|-------------|
| ~~Checkpoint advancement logic~~ | ✅ | `Wal::checkpoint()` scans recovery state, filters committed txns, advances to highest committed LSN, returns `NoNewCheckpoints` when idle. |
| ~~Automatic checkpoint scheduler~~ | ✅ | `Wal::start_checkpoint_scheduler(interval)` — periodic background task calling `checkpoint()` + `truncate_before()`. |
| ~~Benchmarks~~ | ✅ | Throughput benchmarks in `crates/ringwal/benches/wal_throughput.rs` comparing ringwal vs async-wal-db at 1/2/4/8 writer counts via criterion. ringwal scales linearly; async-wal-db is flat. |
| ~~CLI demo tool~~ | ✅ | `bin/demo.rs` (minimal CLI) and `examples/demo.rs` (full lifecycle) showing multi-writer transactions, recovery into `InMemoryStore`, and checkpoint scheduling. |

#### Client-Side (belongs in consumer crate or user code)

| Feature | Rationale |
|---------|-----------|
| ~~In-memory HashMap store~~ | ✅ — `InMemoryStore<K,V>` ships with ringwal (backed by `Arc<RwLock<HashMap>>`). The `WalStore` trait allows external implementations. |
| LMDB storage backend | A persistent storage engine that *uses* the WAL. Should be a separate crate (e.g., `ringwal-lmdb`) depending on ringwal. |
| ~~Apply-to-store on recovery~~ | ✅ — `recover_into_store()` and `apply_transactions()` bridge WAL recovery to any `WalStore` implementation. |

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
│ (ringwal)   │    │  lmdb)      │
└─────────────┘    └─────────────┘

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
6. ~~**Benchmarks**~~ — ✅ done, `benches/wal_throughput.rs` (criterion, ringwal vs async-wal-db)
7. **LMDB storage backend** (separate crate) — depends on #3, lowest priority
8. ~~**CLI demo**~~ — ✅ done, `bin/demo.rs` + `examples/demo.rs`

Only #7 (LMDB) remains.

## Invariants

Formal invariants are documented in [`spec.md`](../spec.md) and enforced via
`debug_assert!` macros in `src/invariants.rs`:

| ID | Invariant |
|----|-----------|
| `INV-WAL-01` | Segment bytes written ≤ `max_segment_size` before rotation |
| `INV-WAL-02` | CRC32 matches header checksum for every entry on read |
| `INV-WAL-03` | Segment IDs are monotonically increasing |
| `INV-WAL-04` | Entry version = 1 (current format) |
| `INV-WAL-05` | Commit waiter notified only after batch containing it is fsynced |
| `INV-WAL-06` | Per-Writer SPSC — each writer is sole producer of its ring |

## Crate Dependencies

```
ringwal
├── ringmpsc-rs          (core SPSC ring buffers)
├── ringmpsc-stream      (async Stream/Sink adapters, SenderFactory)
├── tokio                (async runtime: sync, time, rt, fs, io-util)
├── serde + bincode      (entry serialization)
├── crc32fast            (CRC32 checksums)
└── thiserror            (error derive)
```

## Future Directions

1. **LMDB storage backend** — A persistent storage engine using the WAL, as a
   separate crate (e.g., `ringwal-lmdb`) implementing the `WalStore` trait.
