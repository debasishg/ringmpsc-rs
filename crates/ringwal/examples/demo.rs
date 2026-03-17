//! ringwal demo — concurrent WAL writes with recovery and checkpoint scheduling.
//!
//! Demonstrates all major features:
//! 1. Multi-writer concurrent transactions
//! 2. Crash recovery replayed into an in-memory store
//! 3. Checkpoint + segment truncation (cleaning up old WAL data)
//!
//! Usage: `cargo run -p ringwal --release --example demo`

use ringwal::{
    recover_into_store, InMemoryStore, RealIo, Transaction, Wal, WalConfig,
};
use std::time::{Duration, Instant};

const NUM_WRITERS: usize = 4;
const TXN_PER_WRITER: usize = 200;

#[tokio::main]
async fn main() -> Result<(), ringwal::WalError> {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let config = WalConfig::new(dir.path())
        .with_ring_bits(12) // 4096 slots
        .with_max_writers(NUM_WRITERS)
        .with_max_segment_size(64 * 1024) // 64KB segments for visible rotation
        .with_flush_interval(Duration::from_millis(5))
        .with_batch_hint(128);

    println!("=== ringwal demo ===");
    println!(
        "WAL dir: {}  |  writers: {}  |  txn/writer: {}",
        dir.path().display(),
        NUM_WRITERS,
        TXN_PER_WRITER
    );

    // ── Phase 1: Open WAL and write concurrently ──
    let (mut wal, factory) = Wal::open::<String, Vec<u8>>(config, RealIo)?;

    let start = Instant::now();
    let mut handles = Vec::new();

    for writer_id in 0..NUM_WRITERS {
        let writer = factory.register()?;
        handles.push(tokio::spawn(async move {
            for i in 0..TXN_PER_WRITER {
                let mut tx = Transaction::new();
                let key = format!("w{writer_id}-k{i}");
                let value = format!("writer-{writer_id}-value-{i}").into_bytes();
                tx.insert(key, value);
                tx.commit(&writer).await.expect("commit failed");
            }
        }));
    }

    for h in handles {
        h.await.expect("writer task panicked");
    }
    let elapsed = start.elapsed();

    let total_txn = NUM_WRITERS * TXN_PER_WRITER;
    println!(
        "\n[write]   {} txn in {:.2?}  ({:.0} txn/sec)",
        total_txn,
        elapsed,
        total_txn as f64 / elapsed.as_secs_f64()
    );

    wal.shutdown().await?;
    println!("[shutdown] clean shutdown");

    let seg_count_before = count_segments(dir.path());
    println!("[segments] {seg_count_before} segment files on disk");

    // ── Phase 2: Recover into in-memory store ──
    let start = Instant::now();
    let mut store = InMemoryStore::<String, Vec<u8>>::new();
    let stats = recover_into_store::<String, Vec<u8>, _, _>(dir.path(), &mut store, &RealIo)?;
    let elapsed = start.elapsed();

    println!(
        "\n[recover] {:.2?}  — committed: {}, aborted: {}, incomplete: {}",
        elapsed, stats.committed, stats.aborted, stats.incomplete
    );

    let snapshot = store.snapshot();
    println!(
        "[store]   {} keys recovered (expected {})",
        snapshot.len(),
        total_txn
    );

    // Spot-check
    let mut ok = true;
    for w in 0..NUM_WRITERS {
        let key = format!("w{w}-k0");
        let expected = format!("writer-{w}-value-0").into_bytes();
        match snapshot.get(&key) {
            Some(v) if *v == expected => {}
            Some(v) => { eprintln!("  MISMATCH {key}: got {} bytes", v.len()); ok = false; }
            None => { eprintln!("  MISSING {key}"); ok = false; }
        }
    }
    if ok && snapshot.len() == total_txn {
        println!("  All {total_txn} keys verified.");
    }

    // ── Phase 3: Checkpoint + segment truncation ──
    // In production the store would be persisted; here we simply show that
    // checkpointing allows old segments to be cleaned up.
    let (mut wal2, _) = Wal::open::<String, Vec<u8>>(
        WalConfig::new(dir.path()).with_ring_bits(10).with_max_writers(1),
        RealIo,
    )?;

    match wal2.checkpoint::<String, Vec<u8>>() {
        Ok(lsn) => println!("\n[ckpt]    checkpoint at LSN {lsn}"),
        Err(ringwal::WalError::NoNewCheckpoints) => println!("\n[ckpt]    all caught up"),
        Err(e) => eprintln!("\n[ckpt]    error: {e}"),
    }

    wal2.shutdown().await?;

    let seg_count_after = count_segments(dir.path());
    println!(
        "[truncate] segments: {} -> {} (removed {})",
        seg_count_before,
        seg_count_after,
        seg_count_before.saturating_sub(seg_count_after)
    );

    println!("\n=== demo complete ===");
    Ok(())
}

fn count_segments(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().starts_with("wal-"))
                .count()
        })
        .unwrap_or(0)
}
