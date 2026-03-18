//! ringwal CLI demo — self-contained binary showing the full WAL lifecycle.
//!
//! Usage: `cargo run -p ringwal-store --release --bin ringwal-demo`
//!
//! This binary is the equivalent of async-wal-db's `main.rs`.

use ringwal::{RealIo, Transaction, Wal, WalConfig};
use ringwal_store::{recover_into_store, InMemoryStore};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), ringwal::WalError> {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/ringwal-demo".to_string());
    let dir = std::path::PathBuf::from(dir);

    // Ensure clean start
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("failed to clean demo dir");
    }
    std::fs::create_dir_all(&dir).expect("failed to create demo dir");

    println!("ringwal CLI demo — WAL dir: {}", dir.display());

    // ── Open WAL ──
    let config = WalConfig::new(&dir)
        .with_ring_bits(12)
        .with_max_writers(8)
        .with_max_segment_size(64 * 1024)
        .with_flush_interval(Duration::from_millis(5))
        .with_batch_hint(128);

    let (mut wal, factory) = Wal::open::<String, Vec<u8>>(config, RealIo)?;

    // Start checkpoint scheduler (every 1s for demo)
    wal.start_checkpoint_scheduler::<String, Vec<u8>>(Duration::from_secs(1));

    let factory = Arc::new(factory);

    // ── Concurrent transactions (matching async-wal-db's main.rs pattern) ──
    let f1 = Arc::clone(&factory);
    let handle1 = tokio::spawn(async move {
        let writer = f1.register().expect("register writer 1");
        let mut tx = Transaction::new();
        tx.insert("key1".to_string(), b"value1".to_vec());
        tx.commit(&writer).await.expect("commit tx1");
        println!("  writer-1: committed key1=value1");
    });

    let f2 = Arc::clone(&factory);
    let handle2 = tokio::spawn(async move {
        let writer = f2.register().expect("register writer 2");
        let mut tx = Transaction::new();
        tx.insert("key2".to_string(), b"value2".to_vec());
        tx.commit(&writer).await.expect("commit tx2");
        println!("  writer-2: committed key2=value2");
    });

    handle1.await.expect("writer-1 panicked");
    handle2.await.expect("writer-2 panicked");

    // ── Manual checkpoint ──
    match wal.checkpoint::<String, Vec<u8>>() {
        Ok(tx_id) => println!("Manual checkpoint completed at tx_id: {tx_id}"),
        Err(ringwal::WalError::NoNewCheckpoints) => {
            println!("No new transactions to checkpoint");
        }
        Err(e) => return Err(e),
    }

    wal.shutdown().await?;

    // ── Recover and verify ──
    let mut store = InMemoryStore::<String, Vec<u8>>::new();
    let _stats = recover_into_store::<String, Vec<u8>, _, _>(&dir, &mut store, &RealIo)?;

    let snapshot = store.snapshot();
    println!(
        "Final store keys: {:?}",
        snapshot.keys().collect::<Vec<_>>()
    );
    println!(
        "Demo complete! Check {} for WAL segment files.",
        dir.display()
    );

    Ok(())
}
