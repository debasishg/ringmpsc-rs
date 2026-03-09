# Pipelined Fsync — Design & Implementation

## Summary

`SyncMode::Pipelined` overlaps the *previous* batch's fsync with the *next*
batch's drain+write, while preserving strict durability (INV-WAL-05). Zero
changes to existing `Full`/`DataOnly`/`Background`/`None` code paths.

## Motivation

The current `Background` mode offloads `sync_all()` to `spawn_blocking` but
**still awaits the result** before draining the next batch. This serialises
every batch behind its fsync:

```
 Background:  [write₁][───fsync₁───][write₂][───fsync₂───]
```

With pipelining, we fire-and-forget the previous batch's fsync while
immediately draining and writing the next batch:

```
 Pipelined:   [write₁][write₂──────][write₃──────]
                       [───fsync₁───][───fsync₂───]
```

Commit waiters are only notified after *their* batch's fsync completes —
INV-WAL-05 is preserved.

## Architecture

### Fire-and-forget with direct waiter notification

The key design insight is that the **background fsync thread notifies commit
waiters directly**, not through the flusher. This eliminates all flusher-side
tracking of in-flight fsyncs:

```
Flusher                   Blocking thread pool
───────                   ────────────────────
write_batch(N)
flush_and_clone_fd() ──→  spawn_blocking {
                              fd.sync_all()          // fsync batch N
                              for w in waiters {     // notify batch N's
                                  w.send(())         // commit waiters
                              }
                          }
write_batch(N+1)          ← runs concurrently
flush_and_clone_fd() ──→  spawn_blocking { ... }
...
```

The flusher uses `JoinSet::spawn_blocking` rather than raw `spawn_blocking`.
The `JoinSet` is only used for shutdown cleanup — during normal operation the
flusher never polls it. On shutdown, `while in_flight.join_next().await.is_some() {}`
ensures all in-flight fsyncs complete before the flusher returns.

### Why not VecDeque + drain? (rejected approach)

An earlier design tracked in-flight fsyncs in a `VecDeque<PendingSync>` and
required the flusher to drain completed fsyncs before accepting new data.
This had two problems:

1. **Deadlock**: when `drained == 0` and all writers are blocked waiting on
   their commit notifications, calling `drain_all_pending` before
   `receiver.next()` serialised the pipeline to depth 1.

2. **Select! overhead**: racing `receiver.next()` against the oldest pending
   fsync added ~15–20% overhead from the `tokio::select!` branch.

The fire-and-forget design eliminates both problems — the flusher never
inspects or awaits in-flight fsyncs during normal operation.

### Key invariant: INV-WAL-05 (Commit Durability)

> A commit waiter is only notified after the batch containing its commit marker
> has been written to the active segment and fsynced.

Each `spawn_blocking` call captures its batch's waiters and only notifies them
after `fd.sync_all()` succeeds. Batch N+1's write overlaps with batch N's
fsync, but batch N's waiters are never notified early.

### Why `flush_and_notify` cannot implement pipelining

`flush_and_notify()` is **monolithic**: it writes a batch, fsyncs, and notifies
waiters in one call. Pipelining requires the flusher to return immediately
after the write and delegate fsync+notify to a background thread. The pipelined
mode uses `pipelined_flush()` — a separate code path in `flusher_task` that
calls `write_batch` and `flush_and_clone_fd` directly, then spawns on a
`JoinSet`.

## Implementation

### `pipelined_flush` (wal.rs)

```rust
async fn pipelined_flush(
    segment_mgr: &mut SegmentManager,
    batch: &mut Vec<(u64, Vec<u8>)>,
    commit_waiters: &mut Vec<oneshot::Sender<()>>,
    in_flight: &mut tokio::task::JoinSet<()>,
) {
    if batch.is_empty() { return; }

    // 1. Write batch to active segment (may rotate)
    segment_mgr.write_batch(batch)?;

    // 2. Flush BufWriter + dup() the fd for background sync
    let fd = segment_mgr.flush_and_clone_fd()?;

    // 3. Fire-and-forget: fsync + notify waiters from blocking thread
    let waiters = std::mem::take(commit_waiters);
    in_flight.spawn_blocking(move || {
        fd.sync_all().expect("fsync");
        for tx in waiters { let _ = tx.send(()); }
    });

    batch.clear();
}
```

### `flusher_task` changes

The flusher creates a `JoinSet<()>` local variable. At each flush point it
branches on `sync_mode`:

```rust
if sync_mode == SyncMode::Pipelined {
    pipelined_flush(&mut segment_mgr, &mut batch, &mut commit_waiters, &mut in_flight).await;
} else {
    flush_and_notify(&mut segment_mgr, &mut batch, &mut commit_waiters, sync_mode).await;
}
```

Shutdown + stream-end paths call `pipelined_flush` for any remaining batch,
then drain the JoinSet:

```rust
while in_flight.join_next().await.is_some() {}
```

### `flush_and_notify` exhaustive match

```rust
SyncMode::Pipelined => unreachable!("pipelined mode handled in flusher_task")
```

### SyncMode variant (config.rs)

```rust
/// **Pipelined**: fire-and-forget the *previous* batch's fsync while
/// draining/writing the *next* batch. Strict durability preserved.
/// Requires `multi_thread` runtime.
Pipelined,
```

## Benchmark: Streaming Pipeline

The `wal_streaming_pipeline` benchmark group exercises the pipelined path with
a workload where writers continuously push fire-and-forget `append()` calls
with periodic `commit()`. This keeps the flusher pipeline saturated:

- **50 commits per writer × 100 entries per commit** (fire-and-forget appends)
- **Writer counts: 4, 8, 16, 32** (sweeps concurrency)
- **Side-by-side**: Full vs Background vs Pipelined
- **`batch_hint=2048`, `max_writers=64`**

### Results (Apple Silicon NVMe, macOS)

| Writers | Full (Kelem/s) | Background (Kelem/s) | Pipelined (Kelem/s) | Pipelined vs Full |
|---------|----------------|----------------------|---------------------|-------------------|
| 4       | 58             | 58                   | 52                  | −10%              |
| 8       | 109            | 113                  | 97                  | −11%              |
| 16      | 184            | 183                  | 176                 | −4%               |
| 32      | 220            | 216                  | **283**             | **+29%**          |

**Pipelining delivers a 29% improvement at 32 writers.** Below 16 writers, the
`spawn_blocking` cross-thread overhead exceeds the overlap savings on NVMe
(fsync ~50–100μs). On slower storage (spinning disk, network-attached), the
crossover would occur at lower writer counts.

### Why commit-and-wait benchmarks don't show the benefit

The original `wal_durable` benchmark uses 1-entry-per-commit with `commit().await`
— every writer blocks on fsync before sending the next entry. When all writers
are blocked, there is no data to overlap with the fsync:

```
 Writer A:  [append+commit][──wait──][append+commit][──wait──]
 Flusher:   [write₁][fsync₁]  idle   [write₂][fsync₂]  idle
```

The streaming workload breaks this by decoupling append from commit:

```
 Writer A:  [append×100][commit][append×100][commit]...
 Writer B:  [append×100][commit][append×100][commit]...  (staggered)
 Flusher:   [write₁][write₂──────][write₃──────]
                     [───fsync₁───][───fsync₂───]
```

## Design Decisions

| Decision | Rationale |
|----------|-----------|
| Fire-and-forget `spawn_blocking` | Eliminates flusher-side tracking, zero overhead during normal operation |
| Direct waiter notification from fsync thread | No flusher involvement needed — maximum overlap |
| `JoinSet` for shutdown only | Normal-path flusher never polls pending fsyncs |
| Pipelined bypasses `flush_and_notify` | Monolithic write→sync→notify is incompatible with async delegation |
| Existing paths untouched | `Full`/`DataOnly`/`Background`/`None` — identical code, zero regression risk |
| INV-WAL-05 preserved | Waiters notified only after *their own* batch fsync completes |

## Segment Rotation Safety

If the active segment rotates between batch N and N+1, the cloned fd from N
still refers to the old inode — `sync_all()` on it is correct. This is the
same guarantee as `Background` mode.

## Integration Tests

Two pipelined-specific tests with `#[tokio::test(flavor = "multi_thread")]`:

- `pipelined_multi_writer_commits` — 4 writers × 50 txns, verifies recovery
- `pipelined_single_writer_commit` — 1 writer × 10 txns, verifies recovery

## Verification

```bash
cargo build -p ringwal --release              # compiles (exhaustive matches)
cargo test -p ringwal --release               # 14/14 pass (including pipelined)
cargo bench -p ringwal -- wal_streaming       # side-by-side comparison
cargo clippy -p ringwal                       # no new warnings
```
