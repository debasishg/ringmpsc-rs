# Pipelined Fsync — Improvement Plan

> **Status as of 2026-03-27**:
> - Phase 1 (`PipelinedDataOnly`) — ✅ complete
> - Phase 2 (tuning sweep benchmark) — ✅ complete
> - Phase 3 (`PipelinedDedicated`) — ✅ complete
> - Phase 4 (fsync histogram) — DEFERRED
> - Phase 5 (larger payload benchmarks) — ✅ complete

Five incremental improvements to ringwal's pipelined fsync, implemented as
independent phases. Phase 4 (fsync latency histogram) is deferred.

## Phase 1: `SyncMode::PipelinedDataOnly` — fdatasync variant

**Goal**: Use `fd.sync_data()` (fdatasync) instead of `fd.sync_all()` in the
fire-and-forget path. Skips metadata updates (atime/mtime/size) — potentially
30–80% faster on Linux/ext4 for append-mostly workloads. On macOS APFS the
two are equivalent, so the benchmark will confirm.

**Changes**:

- Add `PipelinedDataOnly` variant to `SyncMode` in `config.rs`
- Modify `pipelined_flush` to accept `SyncMode` and branch on
  `sync_data()` vs `sync_all()` inside `spawn_blocking`
- `flusher_task`: dispatch `PipelinedDataOnly` alongside `Pipelined`
- `flush_and_notify`: `PipelinedDataOnly => unreachable!(…)`
- Benchmark: add to `modes` array in `bench_streaming_pipeline`
- Integration test: `pipelined_data_only_multi_writer_commits`
- Update `spec.md` INV-WAL-05

## Phase 2: Pipelined tuning sweep benchmark

**Goal**: New benchmark group sweeping `flush_interval × batch_hint` on
`Pipelined` to find the optimal overlap point. **No library code changes.**

- New `bench_pipelined_tuning` in `wal_throughput.rs`
- Streaming workload, fixed 16 writers
- Sweep: flush_interval ∈ {1, 3, 5, 10 ms} × batch_hint ∈ {2048, 4096, 8192, 16384}
- Modes: `Pipelined` + `PipelinedDataOnly`
- Group: `wal_pipelined_tuning`

## Phase 3: `SyncMode::PipelinedDedicated` — dedicated OS thread

**Goal**: Replace Tokio's `spawn_blocking` pool with a single `std::thread`
+ bounded `std::sync::mpsc::sync_channel(4)`. Lower tail latency at high
writer counts.

**Changes**:

- Add `PipelinedDedicated` variant to `SyncMode`
- New `dedicated_flush()` that sends `(fd, waiters)` over bounded channel
- Dedicated thread loops on `rx.recv()`, calls `sync_all()`, notifies waiters
- Shutdown: drop sender → thread exits → `flusher_task` joins thread
- `flush_and_notify`: `PipelinedDedicated => unreachable!(…)`
- Benchmark: add to `modes` array
- Integration test: `pipelined_dedicated_multi_writer_commits`
- **No new deps** — uses `std::sync::mpsc::sync_channel`

## Phase 4: DEFERRED — Fsync latency histogram

Deferred. Future work: `Instant::now()` → elapsed inside spawn_blocking,
feed into lightweight histogram, report p50/p95/p99 on shutdown.

## Phase 5: Larger payload streaming benchmarks

**Goal**: Shift pressure from fsync rate toward bandwidth with 1 KiB / 4 KiB
payloads. **Benchmark-only.**

- New group `bench_streaming_payload`
- Payload sizes: {64, 1024, 4096} bytes
- Writers: {4, 8, 16, 32}
- Modes: {Full, Pipelined, PipelinedDataOnly}
- Group: `wal_streaming_payload`

## Parallelism

- Phase 1 ∥ Phase 3 — independent
- Phase 2 depends mildly on Phase 1 (PipelinedDataOnly in sweep)
- Phase 5 depends on Phase 1

## Verification

```bash
cargo build -p ringwal --release
cargo test -p ringwal --release
cargo clippy -p ringwal
cargo bench -p ringwal -- wal_streaming_pipeline
cargo bench -p ringwal -- wal_pipelined_tuning
cargo bench -p ringwal -- wal_streaming_payload
```
