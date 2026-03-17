//! Throughput benchmarks for ringwal.
//!
//! Six benchmark groups:
//!
//! 1. **`wal_durable`** — ringwal with `SyncMode::Full` (sync fsync) on
//!    a `multi_thread` runtime. Sweeps 1/2/4/8 writers.
//!
//! 2. **`wal_durable_bg`** — ringwal with `SyncMode::Background` (fsync
//!    offloaded to `spawn_blocking`). Same writer sweep. Shows the
//!    benefit of not blocking the flusher task during fsync.
//!
//! 3. **`wal_streaming_pipeline`** — streaming workload comparing Full,
//!    Background, Pipelined, `PipelinedDataOnly`, and `PipelinedDedicated`
//!    sync modes side-by-side. Writers use fire-and-forget `append()`
//!    with periodic `commit()`, so the flusher pipeline stays saturated.
//!
//! 4. **`wal_durable_tuning`** — `flush_interval` × `batch_hint` sweep on
//!    `SyncMode::Background` with 4 writers. Shows batching
//!    amortisation under different configurations.
//!
//! 5. **`wal_pipelined_tuning`** — `flush_interval` × `batch_hint` sweep on
//!    `Pipelined` and `PipelinedDataOnly` with 16 writers.
//!
//! 6. **`wal_throughput`** — `SyncMode::None` on `current_thread`.
//!    Payload (16/64/256/1024 B) × writer (1/2/4/8) sweep.
//!
//! 7. **`wal_throughput_mt`** — Same as (6) on `multi_thread`.
//!
//! 8. **`wal_streaming_payload`** — Streaming workload with larger
//!    payloads (64/1024/4096 B) × writers (4/8/16/32) comparing Full,
//!    Pipelined, and `PipelinedDataOnly`.
//!
//! 9. **`wal_direct_io`** — Compares sync modes with and without direct I/O
//!    (macOS `F_NOCACHE` / Linux `O_DIRECT`). Uses 4 KiB payloads matching
//!    filesystem block size. Sweeps 1/4/8 writers across Full, `DataOnly`,
//!    and `PipelinedDataOnly` — each with a `+DirectIO` variant.
//!
//! All groups use a shared runtime per benchmark function (via
//! `b.to_async(&rt)`). Each iteration gets a fresh `TempDir` via
//! `iter_batched`, which auto-cleans on drop.
//!
//! Run: `cargo bench -p ringwal`

use criterion::{
    criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput,
};
use ringwal::SyncMode;
use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

const DURABLE_TXN: u64 = 2_000; // increased for statistical stability with multi_thread
const THROUGHPUT_TXN: u64 = 5_000; // no fsync → more samples for statistical accuracy

// Streaming benchmark: many appends per commit to keep the flusher pipeline full
const STREAMING_COMMITS: u64 = 50; // commits per writer
const STREAMING_ENTRIES: usize = 100; // fire-and-forget appends per commit

// ── helpers ──────────────────────────────────────────────────────────────────

fn make_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn make_mt_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

// ── ringwal configs ─────────────────────────────────────────────────────────

fn ringwal_config_durable(dir: &std::path::Path) -> ringwal::WalConfig {
    ringwal::WalConfig::new(dir)
        .with_ring_bits(14)
        .with_max_writers(16)
        .with_max_segment_size(256 * 1024 * 1024)
        .with_flush_interval(Duration::from_millis(1))
        .with_batch_hint(512)
        .with_sync_mode(SyncMode::Full)
}

fn ringwal_config_background(dir: &std::path::Path) -> ringwal::WalConfig {
    ringwal::WalConfig::new(dir)
        .with_ring_bits(14)
        .with_max_writers(16)
        .with_max_segment_size(256 * 1024 * 1024)
        .with_flush_interval(Duration::from_millis(1))
        .with_batch_hint(512)
        .with_sync_mode(SyncMode::Background)
}

fn ringwal_config_fast(dir: &std::path::Path) -> ringwal::WalConfig {
    ringwal::WalConfig::new(dir)
        .with_ring_bits(14)
        .with_max_writers(16)
        .with_max_segment_size(256 * 1024 * 1024)
        .with_flush_interval(Duration::from_micros(100))
        .with_batch_hint(512)
        .with_sync_mode(SyncMode::None)
}

fn ringwal_config_direct_io(dir: &std::path::Path, mode: SyncMode) -> ringwal::WalConfig {
    ringwal::WalConfig::new(dir)
        .with_ring_bits(14)
        .with_max_writers(32)
        .with_max_segment_size(256 * 1024 * 1024)
        .with_flush_interval(Duration::from_millis(1))
        .with_batch_hint(512)
        .with_sync_mode(mode)
        .with_direct_io(true)
}

// ── ringwal bench routines ──────────────────────────────────────────────────

/// Original 1-entry-per-commit benchmark (commit-and-wait pattern).
async fn ringwal_bench(
    num_writers: usize,
    txn_per_writer: u64,
    payload_size: usize,
    config: ringwal::WalConfig,
) {
    let payload = black_box(vec![0u8; payload_size]);
    let (mut wal, factory) = ringwal::Wal::open::<String, Vec<u8>>(config, ringwal::RealIo).unwrap();

    let mut handles = Vec::new();
    for w in 0..num_writers {
        let writer = factory.register().unwrap();
        let payload = payload.clone();
        handles.push(tokio::spawn(async move {
            for i in 0..txn_per_writer {
                let mut tx = ringwal::Transaction::new();
                tx.insert(format!("k-{w}-{i}"), payload.clone());
                tx.commit(&writer).await.unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    wal.shutdown().await.unwrap();
}

/// Streaming workload: each writer fires many `append()` calls (non-blocking
/// ring push) then issues a single `commit()` per batch.  With enough
/// concurrent writers some always have pending data, keeping the flusher
/// pipeline saturated between fsyncs.
async fn ringwal_bench_streaming(
    num_writers: usize,
    commits_per_writer: u64,
    entries_per_commit: usize,
    payload_size: usize,
    config: ringwal::WalConfig,
) {
    let payload = black_box(vec![0u8; payload_size]);
    let (mut wal, factory) = ringwal::Wal::open::<String, Vec<u8>>(config, ringwal::RealIo).unwrap();

    let mut handles = Vec::new();
    for w in 0..num_writers {
        let writer = factory.register().unwrap();
        let payload = payload.clone();
        handles.push(tokio::spawn(async move {
            for c in 0..commits_per_writer {
                let tx_id = ringwal::next_tx_id();
                // Fire-and-forget appends — just ring push, no durability wait
                for i in 0..entries_per_commit {
                    writer
                        .append(ringwal::WalEntry::Insert {
                            tx_id,
                            timestamp: 0,
                            key: format!("k-{w}-{c}-{i}"),
                            value: payload.clone(),
                        })
                        .await
                        .unwrap();
                }
                // Single durable checkpoint per batch
                writer.commit(tx_id).await.unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    wal.shutdown().await.unwrap();
}

// ── Durable benchmarks (fsync per batch, multi_thread) ───────────────────────

/// Compares **ringwal** vs **async-wal-db** under durable (fsync) workloads.
///
/// - **ringwal**: sweeps 1 / 2 / 4 / 8 concurrent writers, each running
///   `DURABLE_TXN` commit-and-wait transactions (64-byte payloads) with
///   `SyncMode::Full`. Demonstrates group-commit scaling with writer count.
/// - **async-wal-db**: single-writer reference point doing the same 2,000
///   durable transactions, serving as a shared-queue baseline.
///
/// Both appear in the same Criterion group (`wal_durable`) so results are
/// reported side-by-side.
fn bench_durable(c: &mut Criterion) {
    let rt = make_mt_rt();
    let mut group = c.benchmark_group("wal_durable");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));
    group.warm_up_time(Duration::from_secs(3));

    // ringwal: sweep writers [1, 2, 4, 8] to show group-commit scaling
    for num_writers in [1, 2, 4, 8] {
        let total_txn = num_writers as u64 * DURABLE_TXN;
        group.throughput(Throughput::Elements(total_txn));

        group.bench_with_input(
            BenchmarkId::new("ringwal", num_writers),
            &num_writers,
            |b, &nw| {
                b.to_async(&rt).iter_batched(
                    || {
                        let dir = tempfile::tempdir().unwrap();
                        let config = ringwal_config_durable(dir.path());
                        (dir, config)
                    },
                    |(_dir, config)| ringwal_bench(nw, DURABLE_TXN, 64, config),
                    BatchSize::SmallInput,
                );
            },
        );
    }

    // async-wal-db: single-writer reference point
    group.throughput(Throughput::Elements(DURABLE_TXN));
    group.bench_function("async-wal-db/1", |b| {
        b.to_async(&rt).iter_batched(
            || tempfile::tempdir().unwrap(),
            |dir| async move {
                let wal_path = dir.path().join("wal.log");
                let wal_str = wal_path.to_str().unwrap().to_string();

                let db = async_wal_db::DatabaseConfig::new(&wal_str)
                    .with_max_queue_size(100_000)
                    .with_flush_interval_ms(1)
                    .build()
                    .await;

                let db_clone = Arc::clone(&db);
                let handle = tokio::spawn(async move {
                    for i in 0..DURABLE_TXN {
                        let mut tx = async_wal_db::Transaction::new();
                        tx.append_op(
                            &db_clone,
                            async_wal_db::WalEntry::Insert {
                                tx_id: 0,
                                timestamp: 0,
                                key: format!("k-0-{i}"),
                                value: vec![0u8; 64],
                            },
                        )
                        .await
                        .unwrap();
                        tx.commit(&db_clone).await.unwrap();
                    }
                });
                handle.await.unwrap();
                db.wal.stop_flusher().await.unwrap();
                drop(dir); // keep TempDir alive until after shutdown
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

// ── Durable benchmarks — background fsync (spawn_blocking) ───────────────────

fn bench_durable_bg(c: &mut Criterion) {
    let rt = make_mt_rt();
    let mut group = c.benchmark_group("wal_durable_bg");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));
    group.warm_up_time(Duration::from_secs(3));

    for num_writers in [1, 2, 4, 8] {
        let total_txn = num_writers as u64 * DURABLE_TXN;
        group.throughput(Throughput::Elements(total_txn));

        group.bench_with_input(
            BenchmarkId::new("ringwal-bg", num_writers),
            &num_writers,
            |b, &nw| {
                b.to_async(&rt).iter_batched(
                    || {
                        let dir = tempfile::tempdir().unwrap();
                        let config = ringwal_config_background(dir.path());
                        (dir, config)
                    },
                    |(_dir, config)| ringwal_bench(nw, DURABLE_TXN, 64, config),
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

// ── Streaming pipeline benchmark — Full vs Background vs Pipelined ───────────
//
// Exercises the pipelined fsync path with a realistic workload:  many writers
// continuously push fire-and-forget appends and commit periodically, so the
// flusher always has a next batch ready while the previous fsync is in-flight.

fn bench_streaming_pipeline(c: &mut Criterion) {
    let rt = make_mt_rt();
    let mut group = c.benchmark_group("wal_streaming_pipeline");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));
    group.warm_up_time(Duration::from_secs(3));

    let modes: &[(&str, SyncMode)] = &[
        ("full", SyncMode::Full),
        ("background", SyncMode::Background),
        ("pipelined", SyncMode::Pipelined),
        ("pipelined-data", SyncMode::PipelinedDataOnly),
        ("pipelined-dedicated", SyncMode::PipelinedDedicated),
    ];

    for num_writers in [4, 8, 16, 32] {
        let total_ops =
            num_writers as u64 * STREAMING_COMMITS * STREAMING_ENTRIES as u64;
        group.throughput(Throughput::Elements(total_ops));

        for &(label, mode) in modes {
            group.bench_with_input(
                BenchmarkId::new(label, num_writers),
                &num_writers,
                |b, &nw| {
                    b.to_async(&rt).iter_batched(
                        || {
                            let dir = tempfile::tempdir().unwrap();
                            let config = ringwal::WalConfig::new(dir.path())
                                .with_ring_bits(14)
                                .with_max_writers(64)
                                .with_max_segment_size(256 * 1024 * 1024)
                                .with_flush_interval(Duration::from_millis(1))
                                .with_batch_hint(2048)
                                .with_sync_mode(mode);
                            (dir, config)
                        },
                        |(_dir, config)| {
                            ringwal_bench_streaming(
                                nw,
                                STREAMING_COMMITS,
                                STREAMING_ENTRIES,
                                64,
                                config,
                            )
                        },
                        BatchSize::SmallInput,
                    );
                },
            );
        }
    }

    group.finish();
}

// ── Durable tuning sweep — flush_interval × batch_hint on Background ─────────

/// Explores how `flush_interval` and `batch_hint` affect durable throughput.
///
/// Fixes 4 writers on `SyncMode::Background` and sweeps flush intervals
/// (1 / 5 / 10 / 20 ms) × batch hints (512 / 2048 / 8192). Larger batch
/// hints amortise the fsync cost over more entries; longer flush intervals
/// let more writes accumulate before the flusher wakes.
fn bench_durable_tuning(c: &mut Criterion) {
    let rt = make_mt_rt();
    let mut group = c.benchmark_group("wal_durable_tuning");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));
    group.warm_up_time(Duration::from_secs(3));

    let num_writers = 4;
    let total_txn = num_writers as u64 * DURABLE_TXN;

    for flush_ms in [1u64, 5, 10, 20] {
        for batch_hint in [512usize, 2048, 8192] {
            group.throughput(Throughput::Elements(total_txn));
            group.bench_with_input(
                BenchmarkId::new(
                    format!("f{flush_ms}ms-b{batch_hint}"),
                    num_writers,
                ),
                &num_writers,
                |b, &nw| {
                    b.to_async(&rt).iter_batched(
                        || {
                            let dir = tempfile::tempdir().unwrap();
                            let config = ringwal::WalConfig::new(dir.path())
                                .with_ring_bits(14)
                                .with_max_writers(16)
                                .with_max_segment_size(256 * 1024 * 1024)
                                .with_flush_interval(Duration::from_millis(flush_ms))
                                .with_batch_hint(batch_hint)
                                .with_sync_mode(SyncMode::Background);
                            (dir, config)
                        },
                        |(_dir, config)| ringwal_bench(nw, DURABLE_TXN, 64, config),
                        BatchSize::SmallInput,
                    );
                },
            );
        }
    }

    group.finish();
}

// ── Ring throughput benchmarks (no fsync, payload × writer sweep) ────────────

/// Measures raw ring-buffer + serialisation throughput **without** fsync.
///
/// Uses `SyncMode::None` on a single-threaded Tokio runtime. Sweeps
/// payload sizes (16 / 64 / 256 / 1024 B) × writer counts (1 / 2 / 4 / 8)
/// to isolate ring push + bincode encoding cost from I/O.
fn bench_ring_throughput(c: &mut Criterion) {
    let rt = make_rt();
    let mut group = c.benchmark_group("wal_throughput");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));
    group.warm_up_time(Duration::from_secs(1));

    for payload_bytes in [16, 64, 256, 1024] {
        for num_writers in [1, 2, 4, 8] {
            let total_txn = num_writers as u64 * THROUGHPUT_TXN;

            group.throughput(Throughput::Elements(total_txn));

            group.bench_with_input(
                BenchmarkId::new(
                    format!("{payload_bytes}B"),
                    num_writers,
                ),
                &num_writers,
                |b, &nw| {
                    b.to_async(&rt).iter_batched(
                        || {
                            let dir = tempfile::tempdir().unwrap();
                            let config = ringwal_config_fast(dir.path());
                            (dir, config)
                        },
                        |(_dir, config)| {
                            ringwal_bench(nw, THROUGHPUT_TXN, payload_bytes, config)
                        },
                        BatchSize::SmallInput,
                    );
                },
            );
        }
    }

    group.finish();
}

// ── Ring throughput benchmarks — multi_thread runtime ────────────────────────

/// Same as `bench_ring_throughput` but on a **multi-threaded** Tokio runtime
/// (4 worker threads). Reveals cross-thread scheduling overhead vs the
/// single-threaded baseline. `SyncMode::None`, same payload × writer sweep.
fn bench_ring_throughput_mt(c: &mut Criterion) {
    let rt = make_mt_rt();
    let mut group = c.benchmark_group("wal_throughput_mt");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));
    group.warm_up_time(Duration::from_secs(1));

    for payload_bytes in [16, 64, 256, 1024] {
        for num_writers in [1, 2, 4, 8] {
            let total_txn = num_writers as u64 * THROUGHPUT_TXN;

            group.throughput(Throughput::Elements(total_txn));

            group.bench_with_input(
                BenchmarkId::new(
                    format!("{payload_bytes}B"),
                    num_writers,
                ),
                &num_writers,
                |b, &nw| {
                    b.to_async(&rt).iter_batched(
                        || {
                            let dir = tempfile::tempdir().unwrap();
                            let config = ringwal_config_fast(dir.path());
                            (dir, config)
                        },
                        |(_dir, config)| {
                            ringwal_bench(nw, THROUGHPUT_TXN, payload_bytes, config)
                        },
                        BatchSize::SmallInput,
                    );
                },
            );
        }
    }

    group.finish();
}

// ── Pipelined tuning sweep — flush_interval × batch_hint on Pipelined ────────

/// Tuning sweep for pipelined sync modes with a streaming workload.
///
/// Fixes 16 writers using fire-and-forget `append()` + periodic `commit()`.
/// Sweeps flush intervals (1 / 3 / 5 / 10 ms) × batch hints (2048 / 4096 /
/// 8192 / 16384) across three modes: `Pipelined`, `PipelinedDataOnly`, and
/// `PipelinedDedicated`. Shows how pipeline depth and batch sizes interact
/// when the flusher is continuously saturated.
fn bench_pipelined_tuning(c: &mut Criterion) {
    let rt = make_mt_rt();
    let mut group = c.benchmark_group("wal_pipelined_tuning");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));
    group.warm_up_time(Duration::from_secs(3));

    let num_writers = 16;
    let total_ops = num_writers as u64 * STREAMING_COMMITS * STREAMING_ENTRIES as u64;

    let modes: &[(&str, SyncMode)] = &[
        ("pipelined", SyncMode::Pipelined),
        ("pipelined-data", SyncMode::PipelinedDataOnly),
        ("pipelined-dedicated", SyncMode::PipelinedDedicated),
    ];

    for flush_ms in [1u64, 3, 5, 10] {
        for batch_hint in [2048usize, 4096, 8192, 16384] {
            for &(label, mode) in modes {
                group.throughput(Throughput::Elements(total_ops));
                group.bench_with_input(
                    BenchmarkId::new(
                        format!("{label}/f{flush_ms}ms-b{batch_hint}"),
                        num_writers,
                    ),
                    &num_writers,
                    |b, &nw| {
                        b.to_async(&rt).iter_batched(
                            || {
                                let dir = tempfile::tempdir().unwrap();
                                let config = ringwal::WalConfig::new(dir.path())
                                    .with_ring_bits(14)
                                    .with_max_writers(64)
                                    .with_max_segment_size(256 * 1024 * 1024)
                                    .with_flush_interval(Duration::from_millis(flush_ms))
                                    .with_batch_hint(batch_hint)
                                    .with_sync_mode(mode);
                                (dir, config)
                            },
                            |(_dir, config)| {
                                ringwal_bench_streaming(
                                    nw,
                                    STREAMING_COMMITS,
                                    STREAMING_ENTRIES,
                                    64,
                                    config,
                                )
                            },
                            BatchSize::SmallInput,
                        );
                    },
                );
            }
        }
    }

    group.finish();
}

// ── Streaming payload sweep — larger payloads shift pressure to bandwidth ────

/// Streaming workload with larger payloads to stress I/O bandwidth.
///
/// Sweeps payload sizes (64 / 1024 / 4096 B) × writer counts (4 / 8 / 16 /
/// 32) across `Full`, `Pipelined`, `PipelinedDataOnly`, and
/// `PipelinedDedicated` sync modes. Larger payloads shift the bottleneck
/// from fsync latency to raw write bandwidth, exposing how each mode
/// handles sustained data throughput.
fn bench_streaming_payload(c: &mut Criterion) {
    let rt = make_mt_rt();
    let mut group = c.benchmark_group("wal_streaming_payload");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));
    group.warm_up_time(Duration::from_secs(3));

    let modes: &[(&str, SyncMode)] = &[
        ("full", SyncMode::Full),
        ("pipelined", SyncMode::Pipelined),
        ("pipelined-data", SyncMode::PipelinedDataOnly),
        ("pipelined-dedicated", SyncMode::PipelinedDedicated),
    ];

    for payload_bytes in [64usize, 1024, 4096] {
        for num_writers in [4, 8, 16, 32] {
            let total_ops =
                num_writers as u64 * STREAMING_COMMITS * STREAMING_ENTRIES as u64;
            group.throughput(Throughput::Elements(total_ops));

            for &(label, mode) in modes {
                group.bench_with_input(
                    BenchmarkId::new(
                        format!("{label}/{payload_bytes}B"),
                        num_writers,
                    ),
                    &num_writers,
                    |b, &nw| {
                        b.to_async(&rt).iter_batched(
                            || {
                                let dir = tempfile::tempdir().unwrap();
                                let config = ringwal::WalConfig::new(dir.path())
                                    .with_ring_bits(14)
                                    .with_max_writers(64)
                                    .with_max_segment_size(256 * 1024 * 1024)
                                    .with_flush_interval(Duration::from_millis(1))
                                    .with_batch_hint(2048)
                                    .with_sync_mode(mode);
                                (dir, config)
                            },
                            |(_dir, config)| {
                                ringwal_bench_streaming(
                                    nw,
                                    STREAMING_COMMITS,
                                    STREAMING_ENTRIES,
                                    payload_bytes,
                                    config,
                                )
                            },
                            BatchSize::SmallInput,
                        );
                    },
                );
            }
        }
    }

    group.finish();
}

// ── Direct I/O benchmark (F_NOCACHE on macOS) ─────────────────────────────

/// Compares sync modes with and without direct I/O (page-cache bypass).
///
/// Uses 4 KiB payloads (matching filesystem block size) per the recommendation
/// that `F_NOCACHE` + `fdatasync` works best with block-aligned writes.
///
/// Sweeps 1 / 4 / 8 / 16 writers across three sync modes, each with a
/// `+DirectIO` variant, so Criterion reports them side-by-side.
fn bench_direct_io(c: &mut Criterion) {
    let rt = make_mt_rt();
    let mut group = c.benchmark_group("wal_direct_io");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));
    group.warm_up_time(Duration::from_secs(3));

    let payload_size = 4096; // 4 KiB — aligned block size

    let modes: &[(&str, SyncMode)] = &[
        ("Full", SyncMode::Full),
        ("DataOnly", SyncMode::DataOnly),
        ("PipelinedDataOnly", SyncMode::PipelinedDataOnly),
    ];

    for &(label, mode) in modes {
        for num_writers in [1, 4, 8, 16] {
            let total_txn = num_writers as u64 * DURABLE_TXN;
            group.throughput(Throughput::Elements(total_txn));

            // Without direct I/O (baseline)
            group.bench_with_input(
                BenchmarkId::new(format!("{label}/{num_writers}w"), "default"),
                &num_writers,
                |b, &nw| {
                    b.to_async(&rt).iter_batched(
                        || {
                            let dir = tempfile::tempdir().unwrap();
                            let config = ringwal::WalConfig::new(dir.path())
                                .with_ring_bits(14)
                                .with_max_writers(32)
                                .with_max_segment_size(256 * 1024 * 1024)
                                .with_flush_interval(Duration::from_millis(1))
                                .with_batch_hint(512)
                                .with_sync_mode(mode);
                            (dir, config)
                        },
                        |(_dir, config)| {
                            ringwal_bench(nw, DURABLE_TXN, payload_size, config)
                        },
                        BatchSize::SmallInput,
                    );
                },
            );

            // With direct I/O (F_NOCACHE on macOS)
            group.bench_with_input(
                BenchmarkId::new(format!("{label}/{num_writers}w"), "direct_io"),
                &num_writers,
                |b, &nw| {
                    b.to_async(&rt).iter_batched(
                        || {
                            let dir = tempfile::tempdir().unwrap();
                            let config = ringwal_config_direct_io(dir.path(), mode);
                            (dir, config)
                        },
                        |(_dir, config)| {
                            ringwal_bench(nw, DURABLE_TXN, payload_size, config)
                        },
                        BatchSize::SmallInput,
                    );
                },
            );
        }
    }

    group.finish();
}

criterion_group!(benches, bench_durable, bench_durable_bg, bench_streaming_pipeline, bench_durable_tuning, bench_pipelined_tuning, bench_ring_throughput, bench_ring_throughput_mt, bench_streaming_payload, bench_direct_io);
criterion_main!(benches);
