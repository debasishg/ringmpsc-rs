# ringwal Benchmark Results

Benchmarks run on macOS (Apple Silicon), criterion 0.5 with 20 samples
and 10 s measurement time per benchmark.

**Methodology**: Each iteration gets a fresh `TempDir` via `iter_batched`,
separating setup (directory + config creation) from the measured routine.
The runtime is created once per benchmark function (via `b.to_async(&rt)`),
eliminating runtime-construction overhead from measurements.

## Summary

### Durable (fsync) — 64-byte payload, `multi_thread` runtime

| WAL | Writers | Throughput | Scaling |
|-----|---------|-----------|---------|
| ringwal | 1 | ~171 txn/s | 1.0× |
| ringwal | 2 | ~337 txn/s | 2.0× |
| ringwal | 4 | ~677 txn/s | 4.0× |
| ringwal | 8 | ~1,338 txn/s | 7.8× |
| async-wal-db | 1 | ~170 txn/s | (reference) |

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

### 4. Background fsync could push durable throughput further — with a data-loss trade-off

The current durable benchmarks show **1,338 txn/s** at 8 writers via
group commit, but fsync still blocks the flusher thread during each batch
flush. Offloading `sync_all()` to a background thread (via
`spawn_blocking()`) would let the flusher immediately start draining the
next batch while the previous one syncs to disk. This would increase
effective batch size and throughput further.

**However, background fsync introduces a data-loss window.** If the
process crashes after `write()` but before `sync_all()` completes, any
writes in that window are lost. The size equals fsync latency (~5 ms).

| Strategy | Throughput (8 writers) | Durability guarantee |
|----------|----------------------|---------------------|
| **Sync per batch** (current) | ~1,338 txn/s | Every committed txn is durable before ack |
| **Background fsync** (projected) | higher (TBD) | Writes acked before fsync — crash can lose last sync window |
| **`SyncMode::None`** (current bench) | ~6,500 txn/s | No durability — flush only, data lost on crash |

### 5. Additional `multi_thread` opportunities

Beyond group-commit scaling (already demonstrated), `multi_thread`
enables:

1. **Background fsync** — the flusher writes to the page cache (fast) and
   spawns `sync_all()` on `spawn_blocking()`. The flusher immediately
   starts the next batch drain while the previous one syncs, increasing
   pipeline depth.
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
Batch hint:        512
Flush interval:    1 ms (durable) / 100 µs (throughput)
Payload sizes:     64 bytes (durable) / 16 / 64 / 256 / 1024 bytes (throughput)
Txns per writer:   500 (durable) / 5,000 (throughput)
Writer counts:     1 / 2 / 4 / 8 (durable + throughput sweeps)
Samples:           20
Measurement time:  10 s
Runtimes:          multi_thread 4 workers (durable + throughput_mt)
                   current_thread (throughput)
Setup isolation:   iter_batched with fresh TempDir per iteration
```

## Running

```bash
# All groups (durable + throughput + throughput_mt)
cargo bench -p ringwal

# current_thread throughput only
cargo bench -p ringwal -- "wal_throughput/"

# multi_thread throughput only
cargo bench -p ringwal -- "wal_throughput_mt"

# Specific payload × writer combo
cargo bench -p ringwal -- "wal_throughput/64B/8"
cargo bench -p ringwal -- "wal_throughput_mt/64B/8"

# Durable only (fsync, slow)
cargo bench -p ringwal -- "wal_durable"
```
