# ringwal Benchmark Results

Benchmarks run on macOS (Apple Silicon), `current_thread` tokio runtime,
criterion 0.5 with 10 samples per benchmark.

## Summary

| Mode | Writers | Throughput | Scaling |
|------|---------|-----------|---------|
| Durable (fsync) | 1 | ~170 txn/s | — |
| No-sync | 1 | ~838 txn/s | 1.0× |
| No-sync | 2 | ~1,666 txn/s | 1.99× |
| No-sync | 4 | ~3,310 txn/s | 3.95× |
| No-sync | 8 | ~6,543 txn/s | 7.81× |

async-wal-db durable throughput: ~171 txn/s (single writer) — statistically identical to ringwal under fsync.

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
its strength: 8 writers reach **7.81× throughput** (vs 8× ideal). Each
producer owns a dedicated SPSC ring with zero contention, so adding writers
adds throughput proportionally.

### 3. `current_thread` runtime bounds per-writer throughput

On a single-threaded runtime the writer and flusher cooperatively
time-share. Each commit round-trip is gated by the flusher's poll interval
(configured at 100 µs, but macOS timer coalescing rounds up to ~1 ms).
This puts a floor of ~838 txn/s per writer. A `multi_thread` runtime would
allow true pipelining (writers fill rings while flusher drains in parallel)
but is currently blocked by a `RingReceiver` notification race in
`ringmpsc-stream` (tracked separately).

### 4. Unlocking higher throughput requires async fsync

The path to high durable throughput on macOS is:
1. **Offload `sync_all()` to a background thread** so writers can keep
   filling rings during the 5 ms fsync window — enabling real group commit
   (one fsync covers hundreds of writes).
2. **Fix the `RingReceiver` waker bug** so the flusher reliably wakes
   after yielding at an await point, enabling `multi_thread` runtime.
3. **Batch coalescing** — with both fixes, a 5 ms fsync window at
   838 writes/writer/sec × 8 writers ≈ 33 writes per fsync, pushing
   durable throughput toward ~6,500 txn/s.

## Benchmark Configuration

```
Ring capacity:     16,384 slots (ring_bits = 14)
Max writers:       16
Segment size:      256 MB
Batch hint:        512
Flush interval:    1 ms (durable) / 100 µs (throughput)
Payload:           64-byte values, ~10-char keys
Txns per writer:   500 (durable) / 5,000 (throughput)
```

## Running

```bash
# Both groups (durable is slow — ~5 min for 10 samples)
cargo bench -p ringwal

# Throughput only (no-fsync, fast)
cargo bench -p ringwal -- "wal_throughput"

# Durable only (fsync, slow)
cargo bench -p ringwal -- "wal_durable"
```
