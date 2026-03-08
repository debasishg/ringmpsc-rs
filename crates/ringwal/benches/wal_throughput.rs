//! Throughput benchmarks for ringwal vs async-wal-db.
//!
//! Benchmarks:
//! - Single-writer throughput
//! - Multi-writer scaling (1, 2, 4, 8 writers)
//!
//! Uses `current_thread` tokio runtime per iteration because
//! `RingReceiver`'s Stream impl has a notification race under
//! multi-thread runtimes (tracked separately).
//!
//! Run: `cargo bench -p ringwal`

use criterion::{
    criterion_group, criterion_main, BenchmarkId, Criterion, Throughput,
};
use std::sync::Arc;
use std::time::Duration;

const TXN_PER_WRITER: u64 = 500;

// ── ringwal helpers ───────────────────────────────────────────────────────────

fn ringwal_config(dir: &std::path::Path) -> ringwal::WalConfig {
    ringwal::WalConfig::new(dir)
        .with_ring_bits(14) // 16384 slots
        .with_max_writers(16)
        .with_max_segment_size(256 * 1024 * 1024) // 256MB — avoid rotation overhead
        .with_flush_interval(Duration::from_millis(1))
        .with_batch_hint(512)
}

async fn ringwal_bench(num_writers: usize, txn_per_writer: u64) {
    let dir = tempfile::tempdir().unwrap();
    let config = ringwal_config(dir.path());
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

// ── Criterion benchmarks ─────────────────────────────────────────────────────

fn bench_single_writer(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_single_writer");
    group.throughput(Throughput::Elements(TXN_PER_WRITER));
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(3));

    group.bench_function("ringwal", |b| {
        b.iter(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(ringwal_bench(1, TXN_PER_WRITER));
        });
    });

    group.bench_function("async-wal-db", |b| {
        b.iter(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async_wal_db_bench(1, TXN_PER_WRITER));
        });
    });

    group.finish();
}

fn bench_multi_writer_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_scaling");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(3));

    for num_writers in [1, 2, 4, 8] {
        let total_txn = num_writers as u64 * TXN_PER_WRITER;
        group.throughput(Throughput::Elements(total_txn));

        group.bench_with_input(
            BenchmarkId::new("ringwal", num_writers),
            &num_writers,
            |b, &nw| {
                b.iter(|| {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    rt.block_on(ringwal_bench(nw, TXN_PER_WRITER));
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("async-wal-db", num_writers),
            &num_writers,
            |b, &nw| {
                b.iter(|| {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    rt.block_on(async_wal_db_bench(nw, TXN_PER_WRITER));
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_single_writer, bench_multi_writer_scaling);
criterion_main!(benches);
