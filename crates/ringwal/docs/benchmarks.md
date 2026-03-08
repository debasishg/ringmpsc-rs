# ringwal Benchmark Results

Benchmarks run on macOS (Apple Silicon), criterion 0.5 with 20 samples
and 10 s measurement time per benchmark.

**Methodology**: Each iteration gets a fresh `TempDir` via `iter_batched`,
separating setup (directory + config creation) from the measured routine.
The runtime is created once per benchmark function (via `b.to_async(&rt)`),
eliminating runtime-construction overhead from measurements.

## Summary

### Durable (fsync) — 64-byte payload

| WAL | Writers | Throughput | Notes |
|-----|---------|-----------|-------|
| ringwal | 1 | ~170 txn/s | fsync-bound |
| async-wal-db | 1 | ~171 txn/s | statistically identical |

### No-sync — `current_thread` vs `multi_thread` (64-byte payload)

| Writers | `current_thread` | `multi_thread` | Diff |
|---------|-----------------|----------------|------|
| 1 | ~838 elem/s | ~821 elem/s | −2% |
| 2 | ~1,666 elem/s | ~1,586 elem/s | −5% |
| 4 | ~3,311 elem/s | ~2,973 elem/s | −10% |
| 8 | ~6,563 elem/s | ~6,229 elem/s | −5% |

*(16 B, 256 B, 1024 B rows will be populated after running the payload sweep.)*

## Key Takeaways

### 1. fsync dominates durable-commit latency

macOS `F_FULLFSYNC` (the only correct fsync on APFS) costs ~5 ms per call.
With `current_thread` scheduling the flusher blocks all writers during
`sync_all()`, producing batch sizes of 1 and capping throughput at
**~170 txn/s** regardless of WAL implementation. Both ringwal and
async-wal-db hit the same ceiling — proving the bottleneck is the kernel,
not the WAL design.

### 2. Ring decomposition delivers near-linear writer scaling

With fsync removed (`SyncMode::None`), the ring buffer architecture shows
its strength: 8 writers reach **7.81× throughput** (vs 8× ideal) on
`current_thread`. Each producer owns a dedicated SPSC ring with zero
contention, so adding writers adds throughput proportionally.

### 3. `multi_thread` runtime adds overhead without unlocking parallelism

The `multi_thread` benchmarks (`wal_throughput_mt`) are **2–10% slower**
than `current_thread` across all writer counts. The reason: the bottleneck
is the **flusher's poll interval** (100 µs configured, ~1 ms effective on
macOS due to timer coalescing), not cooperative scheduling. The flusher is
the single serialization point — it must drain all rings, serialize, and
write to disk. On `multi_thread`, cross-thread communication (wake
latency, cache-line bouncing on ring atomics) adds overhead without
enabling true pipelining.

The `current_thread` runtime is actually *ideal* for this workload because
writers and flusher share a single event loop with zero cross-thread
synchronization overhead. The near-linear scaling (7.81×/8×) proves the
ring decomposition eliminates contention within the single thread.

### 4. Unlocking higher durable throughput requires background fsync — with a data-loss trade-off

The path to high durable throughput on macOS is to offload `sync_all()` to
a background thread so writers can keep filling rings during the 5 ms
fsync window, enabling real group commit (one fsync covers many writes).

**However, background fsync introduces a data-loss window.** If the
process crashes after `write()` but before `sync_all()` completes, any
writes in that window are lost — the OS page cache holds the data but it
hasn't reached stable storage. The size of the window equals the fsync
latency (~5 ms on macOS), so at 838 writes/writer/sec × 8 writers, up to
**~33 committed transactions could be lost on crash**.

This is a fundamental trade-off:

| Strategy | Throughput | Durability guarantee |
|----------|-----------|---------------------|
| **Sync per batch** (current) | ~170 txn/s | Every committed txn is durable before ack |
| **Background fsync** | ~6,500 txn/s (projected) | Writes acked before fsync — crash can lose the last fsync window |
| **`SyncMode::None`** (current bench) | ~6,500 txn/s | No durability — flush only, data lost on crash |

The background-fsync approach is what most production WALs use (e.g.,
PostgreSQL's `wal_sync_method = fdatasync` with `commit_delay`). It's
acceptable when:
- The application can tolerate losing the last few milliseconds of writes
- A replication layer provides redundancy (lost writes survive on replicas)
- The commit protocol allows replay from upstream

For applications requiring strict per-transaction durability (e.g.,
financial ledgers without replication), the current synchronous fsync
path is the only correct choice. The ~170 txn/s ceiling is a hardware
constraint, not a software one.

### 5. Where `multi_thread` *would* help

`multi_thread` becomes beneficial when the flusher does expensive work
that genuinely benefits from parallelism with writers:

1. **Background fsync** — the flusher writes to the page cache (fast) and
   spawns `sync_all()` on a blocking thread via `spawn_blocking()`. Writers
   continue filling rings on worker threads during the 5 ms fsync. This
   requires `multi_thread` because `spawn_blocking` needs a thread pool.
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
Payload sizes:     16 / 64 / 256 / 1024 bytes (throughput sweep)
Txns per writer:   500 (durable) / 5,000 (throughput)
Writer counts:     1 / 2 / 4 / 8 (throughput sweep)
Samples:           20
Measurement time:  10 s
Runtimes:          current_thread + multi_thread (4 workers)
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
