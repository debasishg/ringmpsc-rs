# ringwal Benchmark Results

Benchmarks run on macOS (Apple Silicon), criterion 0.5 with 20 samples
and 10 s measurement time per benchmark.

**Methodology**: Each iteration gets a fresh `TempDir` via `iter_batched`,
separating setup (directory + config creation) from the measured routine.
The runtime is created once per benchmark function (via `b.to_async(&rt)`),
eliminating runtime-construction overhead from measurements.

## Summary

### Durable (fsync) — 64-byte payload, `multi_thread` runtime

| WAL | SyncMode | Writers | Throughput | vs Full/1 |
|-----|----------|---------|-----------|----------|
| ringwal | Full | 1 | ~171 txn/s | 1.0× |
| ringwal | Full | 2 | ~337 txn/s | 2.0× |
| ringwal | Full | 4 | ~677 txn/s | 4.0× |
| ringwal | Full | 8 | ~1,359 txn/s | 7.9× |
| ringwal | Background | 1 | ~161 txn/s | 0.9× |
| ringwal | Background | 4 | ~653 txn/s | 3.8× |
| ringwal | Background | 8 | ~1,347 txn/s | 7.9× |
| async-wal-db | — | 1 | ~170 txn/s | (reference) |

### No-sync — `current_thread` vs `multi_thread` (64-byte payload)

| Writers | `current_thread` | `multi_thread` | Diff |
|---------|-----------------|----------------|------|
| 1 | ~838 elem/s | ~821 elem/s | −2% |
| 2 | ~1,666 elem/s | ~1,586 elem/s | −5% |
| 4 | ~3,311 elem/s | ~2,973 elem/s | −10% |
| 8 | ~6,563 elem/s | ~6,229 elem/s | −5% |

*(16 B, 256 B, 1024 B rows will be populated after running the payload sweep.)*

## Key Takeaways

### 1. Group-commit scaling under fsync

macOS `F_FULLFSYNC` (the only correct fsync on APFS) costs ~5 ms per call.
With a single writer, each fsync covers only that writer's batch, capping
throughput at **~170 txn/s**. But on a `multi_thread` runtime, multiple
writers fill their rings in parallel on separate worker threads while the
flusher drains *all* rings into a single batch before issuing one fsync.
This is group commit: one fsync covers N writers' work.

The result is **near-linear scaling**: 8 writers reach **1,338 txn/s**
(7.8× vs 1 writer) with wall-clock time unchanged (~2.95 s). The fsync
cost is constant — more writers just mean more transactions per fsync
batch. async-wal-db at 1 writer matches ringwal's single-writer baseline
(~170 txn/s), confirming fsync is the common bottleneck.

### 2. Ring decomposition delivers near-linear writer scaling (both modes)

The ring-per-producer architecture shows near-linear scaling in both
durable and no-sync modes:

- **Durable (multi_thread)**: 8 writers → 7.8× (group-commit amortization)
- **No-sync (current_thread)**: 8 writers → 7.81× (zero-contention rings)

Each producer owns a dedicated SPSC ring with zero contention, so adding
writers adds throughput proportionally. The flusher drains all rings in
one pass regardless of count.

### 3. Runtime choice depends on workload: `multi_thread` for durable, `current_thread` for no-sync

**Durable (`SyncMode::Full`)**: `multi_thread` is **essential**. On
`current_thread`, `sync_all()` blocks the single thread — writers stall
during the 5 ms fsync, batch sizes collapse to 1, and throughput caps at
~170 txn/s regardless of writer count. On `multi_thread`, writers run on
separate worker threads, filling rings while fsync blocks the flusher's
thread. This enables group commit and the 7.8× scaling seen above.

**No-sync (`SyncMode::None`)**: `current_thread` is 2–10% **faster**.
The bottleneck is the flusher's poll interval (100 µs configured, ~1 ms
effective on macOS due to timer coalescing). Since the flusher is the
single serialization point (drain → serialize → write), cross-thread
wake latency and cache-line bouncing on ring atomics add overhead without
enabling pipelining. Writers and flusher sharing a single event loop with
zero cross-thread overhead is ideal when there's no blocking I/O.

### 4. `SyncMode::Background` — spawn_blocking fsync

`Background` mode offloads `sync_all()` to Tokio's blocking thread pool
via `spawn_blocking()`. The flusher clones the file descriptor (`dup()`)
and awaits the blocking task, yielding the Tokio worker thread so other
tasks (writers) can progress.

**Result**: Background matches Full within noise at all writer counts.
With 8 writers: **~1,347 txn/s** (Background) vs **~1,359 txn/s** (Full).
At 1 writer, Background is ~6% slower due to `spawn_blocking` overhead
(thread switch + schedule).

Why no improvement? The flusher **awaits** the `spawn_blocking` result
before notifying commit waiters — it doesn't pipeline. This preserves
the durability guarantee (every committed txn is fsync'd before ack) but
means the flusher still does one fsync per batch cycle. The win would
come from **fire-and-forget** pipelining (start next drain while previous
fsync runs), which trades strict per-batch durability for throughput.

### 5. Flush interval tuning: shorter is better for durable

Sweep with `SyncMode::Background`, 4 writers, batch_hint 512/2048/8192:

| flush_interval | batch_hint | Throughput |
|---------------|-----------|------------|
| 1 ms | 512 | ~650 txn/s |
| 1 ms | 2048 | ~653 txn/s |
| 1 ms | 8192 | ~641 txn/s |
| 5 ms | 512 | ~401 txn/s |
| 5 ms | 2048 | ~401 txn/s |
| 5 ms | 8192 | ~401 txn/s |
| 10 ms | 512 | ~398 txn/s |
| 10 ms | 2048 | ~398 txn/s |
| 10 ms | 8192 | ~398 txn/s |

**Key finding**: `batch_hint` has no effect — the flusher is always
blocked on fsync, not on draining. `flush_interval` dominates:
with 5 ms fsync per cycle, each cycle takes `flush_interval + fsync`.
At 1 ms interval: ~6 ms/cycle → ~167 fsyncs/s. At 10 ms: ~15 ms/cycle
→ ~67 fsyncs/s. Shorter intervals minimize idle time between fsyncs.

### 6. Additional `multi_thread` opportunities

Beyond group-commit scaling (already demonstrated), `multi_thread`
enables:

1. **Pipelined fsync** — fire-and-forget `spawn_blocking(sync_all)`,
   immediately start draining next batch. Trades per-batch durability
   for higher throughput. Commit waiters notified after the *next* fsync
   completes.
2. **Compression/encryption** — if the flusher must compress or encrypt
   batches before writing, that CPU work can overlap with writers producing
   on separate cores.
3. **Network replication** — streaming WAL segments to replicas can overlap
   with local writes on a multi-threaded executor.

## Benchmark Configuration

```
Ring capacity:     16,384 slots (ring_bits = 14)
Max writers:       16
Segment size:      256 MB
Batch hint:        512 (default), tuning sweep: 512 / 2048 / 8192
Flush interval:    1 ms (durable default), tuning sweep: 1 / 5 / 10 ms
                   100 µs (throughput)
Sync modes:        Full (sync_all), Background (spawn_blocking + sync_all),
                   DataOnly (sync_data), None (flush only)
Payload sizes:     64 bytes (durable) / 16 / 64 / 256 / 1024 bytes (throughput)
Txns per writer:   2,000 (durable) / 5,000 (throughput)
Writer counts:     1 / 2 / 4 / 8
Samples:           20 (10 for tuning sweep)
Measurement time:  10 s
Runtimes:          multi_thread 4 workers (durable + throughput_mt)
                   current_thread (throughput)
Setup isolation:   iter_batched with fresh TempDir per iteration
```

## Running

```bash
# All groups
cargo bench -p ringwal

# Durable: Full sync (fsync per batch)
cargo bench -p ringwal -- "wal_durable/ringwal"

# Durable: Background fsync (spawn_blocking)
cargo bench -p ringwal -- "wal_durable_bg"

# Durable: config tuning sweep (flush_interval × batch_hint)
cargo bench -p ringwal -- "wal_durable_tuning"

# No-sync throughput (current_thread / multi_thread)
cargo bench -p ringwal -- "wal_throughput/"
cargo bench -p ringwal -- "wal_throughput_mt"

# Specific payload × writer combo
cargo bench -p ringwal -- "wal_throughput/64B/8"
cargo bench -p ringwal -- "wal_throughput_mt/64B/8"

# Single writer count (use $ anchor to avoid prefix matching)
cargo bench -p ringwal -- "wal_durable/ringwal/1$"
cargo bench -p ringwal -- "wal_durable_bg/ringwal-bg/8$"
```
