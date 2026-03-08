//! Throughput benchmarks for ringwal.
//!
//! Three benchmark groups:
//!
//! 1. **`wal_durable`** — ringwal and async-wal-db with full `sync_all()`
//!    after every batch. Shows real durable-commit throughput (~170 txn/s
//!    on macOS due to `F_FULLFSYNC` ≈ 5 ms per call). Both implementations
//!    are fsync-bound so numbers are comparable.
//!
//! 2. **`wal_throughput`** — ringwal with `SyncMode::None` on a
//!    `current_thread` runtime. Sweeps payload sizes × writer counts.
//!    Shows cooperative single-threaded throughput ceiling.
//!
//! 3. **`wal_throughput_mt`** — Same as (2) but on a `multi_thread`
//!    runtime. Writers fill rings on worker threads while the flusher
//!    drains in parallel — true pipelining. Enabled by the
//!    `RingReceiver` register-then-recheck fix (INV-STREAM-05/06).
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

const DURABLE_TXN: u64 = 500; // fsync-bound → keep small to avoid hour-long runs
const THROUGHPUT_TXN: u64 = 5_000; // no fsync → more samples for statistical accuracy

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

fn ringwal_config_fast(dir: &std::path::Path) -> ringwal::WalConfig {
    ringwal::WalConfig::new(dir)
        .with_ring_bits(14)
        .with_max_writers(16)
        .with_max_segment_size(256 * 1024 * 1024)
        .with_flush_interval(Duration::from_micros(100))
        .with_batch_hint(512)
        .with_sync_mode(SyncMode::None)
}

// ── ringwal bench routine ───────────────────────────────────────────────────

async fn ringwal_bench(
    num_writers: usize,
    txn_per_writer: u64,
    payload_size: usize,
    config: ringwal::WalConfig,
) {
    let payload = black_box(vec![0u8; payload_size]);
    let (mut wal, factory) = ringwal::Wal::open::<String, Vec<u8>>(config).unwrap();

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

// ── Durable benchmarks (fsync per batch) ─────────────────────────────────────

fn bench_durable(c: &mut Criterion) {
    let rt = make_rt();
    let mut group = c.benchmark_group("wal_durable");
    group.throughput(Throughput::Elements(DURABLE_TXN));
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));
    group.warm_up_time(Duration::from_secs(3));

    group.bench_function("ringwal", |b| {
        b.to_async(&rt).iter_batched(
            || {
                let dir = tempfile::tempdir().unwrap();
                let config = ringwal_config_durable(dir.path());
                (dir, config)
            },
            |(_dir, config)| ringwal_bench(1, DURABLE_TXN, 64, config),
            BatchSize::SmallInput,
        );
    });

    group.bench_function("async-wal-db", |b| {
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

                let mut handles = Vec::new();
                for w in 0..1usize {
                    let db_clone = Arc::clone(&db);
                    handles.push(tokio::spawn(async move {
                        for i in 0..DURABLE_TXN {
                            let mut tx = async_wal_db::Transaction::new();
                            tx.append_op(
                                &db_clone,
                                async_wal_db::WalEntry::Insert {
                                    tx_id: 0,
                                    timestamp: 0,
                                    key: format!("k-{w}-{i}"),
                                    value: vec![0u8; 64],
                                },
                            )
                            .await
                            .unwrap();
                            tx.commit(&db_clone).await.unwrap();
                        }
                    }));
                }
                for h in handles {
                    h.await.unwrap();
                }
                db.wal.stop_flusher().await.unwrap();
                drop(dir); // keep TempDir alive until after shutdown
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

// ── Ring throughput benchmarks (no fsync, payload × writer sweep) ────────────

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

criterion_group!(benches, bench_durable, bench_ring_throughput, bench_ring_throughput_mt);
criterion_main!(benches);
