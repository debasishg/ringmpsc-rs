//! Throughput benchmarks for ringwal vs async-wal-db.
//!
//! Two benchmark groups:
//!
//! 1. **`wal_durable`** — Both WALs with full `sync_all()` after every batch.
//!    Shows real durable-commit throughput (~170 txn/s on macOS due to
//!    `F_FULLFSYNC` ≈ 5 ms per call).  Numbers are comparable because both
//!    implementations are fsync-bound under `current_thread` scheduling.
//!
//! 2. **`wal_throughput`** — ringwal with `SyncMode::None` (flush-only, no
//!    fsync) and aggressive 100µs poll interval at 1/2/4/8 writers.
//!    Shows the ring buffer + serialization + I/O write throughput without
//!    the disk durability bottleneck.
//!
//! **Note on `current_thread`**: Both groups use a single-threaded tokio
//! runtime. The writer and flusher alternate cooperatively, so each commit
//! round-trip is bounded by the flusher's poll interval. This is realistic
//! for embedded / single-core deployments but understates throughput on
//! multi-core systems. A multi-thread runtime would allow true pipelining
//! but is blocked by a known `RingReceiver` notification race (tracked
//! separately).
//!
//! Run: `cargo bench -p ringwal`

use criterion::{
    criterion_group, criterion_main, BenchmarkId, Criterion, Throughput,
};
use ringwal::SyncMode;
use std::sync::Arc;
use std::time::Duration;

const DURABLE_TXN: u64 = 500; // fsync-bound → keep small to avoid hour-long runs
const THROUGHPUT_TXN: u64 = 5_000; // no fsync → more samples for statistical accuracy

// ── ringwal helpers ───────────────────────────────────────────────────────────

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
        .with_flush_interval(Duration::from_micros(100)) // aggressive polling
        .with_batch_hint(512)
        .with_sync_mode(SyncMode::None)
}

async fn ringwal_bench(
    num_writers: usize,
    txn_per_writer: u64,
    config: ringwal::WalConfig,
) {
    let (mut wal, factory) = ringwal::Wal::open::<String, Vec<u8>>(config).unwrap();

    let mut handles = Vec::new();
    for w in 0..num_writers {
        let writer = factory.register().unwrap();
        handles.push(tokio::spawn(async move {
            for i in 0..txn_per_writer {
                let mut tx = ringwal::Transaction::new();
                tx.insert(format!("k-{w}-{i}"), vec![0u8; 64]);
                tx.commit(&writer).await.unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    wal.shutdown().await.unwrap();
}

// ── async-wal-db helpers ──────────────────────────────────────────────────────

async fn async_wal_db_bench(num_writers: usize, txn_per_writer: u64) {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("wal.log");
    let wal_str = wal_path.to_str().unwrap().to_string();

    let db = async_wal_db::DatabaseConfig::new(&wal_str)
        .with_max_queue_size(100_000)
        .with_flush_interval_ms(1)
        .build()
        .await;

    let mut handles = Vec::new();
    for w in 0..num_writers {
        let db_clone = Arc::clone(&db);
        handles.push(tokio::spawn(async move {
            for i in 0..txn_per_writer {
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
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn make_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

// ── Durable benchmarks (fsync per batch) ─────────────────────────────────────

fn bench_durable(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_durable");
    group.throughput(Throughput::Elements(DURABLE_TXN));
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(3));

    group.bench_function("ringwal", |b| {
        b.iter(|| {
            let dir = tempfile::tempdir().unwrap();
            let config = ringwal_config_durable(dir.path());
            make_rt().block_on(ringwal_bench(1, DURABLE_TXN, config));
        });
    });

    group.bench_function("async-wal-db", |b| {
        b.iter(|| {
            make_rt().block_on(async_wal_db_bench(1, DURABLE_TXN));
        });
    });

    group.finish();
}

// ── Ring throughput benchmarks (no fsync) ────────────────────────────────────

fn bench_ring_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_throughput");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));

    for num_writers in [1, 2, 4, 8] {
        let total_txn = num_writers as u64 * THROUGHPUT_TXN;
        group.throughput(Throughput::Elements(total_txn));

        group.bench_with_input(
            BenchmarkId::new("ringwal", num_writers),
            &num_writers,
            |b, &nw| {
                b.iter(|| {
                    let dir = tempfile::tempdir().unwrap();
                    let config = ringwal_config_fast(dir.path());
                    make_rt().block_on(ringwal_bench(nw, THROUGHPUT_TXN, config));
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_durable, bench_ring_throughput);
criterion_main!(benches);
