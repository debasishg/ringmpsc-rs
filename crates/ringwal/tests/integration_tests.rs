//! Integration tests for ringwal.

use ringwal::{
    recover, recover_into_store, read_checkpoint, write_checkpoint, InMemoryStore,
    RecoveryAction, SyncMode, Transaction, Wal, WalConfig, WalEntry,
};
use std::sync::Arc;
use tempfile::TempDir;

fn test_config(dir: &std::path::Path) -> WalConfig {
    WalConfig::new(dir)
        .with_ring_bits(10) // 1024 slots — small for tests
        .with_max_writers(8)
        .with_max_segment_size(1024 * 1024) // 1MB
        .with_flush_interval(std::time::Duration::from_millis(5))
        .with_batch_hint(64)
}

#[tokio::test]
async fn single_writer_commit() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path());
    let (mut wal, factory) = Wal::open::<String, Vec<u8>>(config).unwrap();
    let writer = factory.register().unwrap();

    let mut tx = Transaction::new();
    tx.insert("key1".into(), b"value1".to_vec());
    tx.insert("key2".into(), b"value2".to_vec());
    tx.commit(&writer).await.unwrap();

    wal.shutdown().await.unwrap();

    // Recover and verify
    let (recovered, stats) = recover::<String, Vec<u8>>(tmp.path()).unwrap();
    assert_eq!(stats.committed, 1);
    assert_eq!(stats.aborted, 0);
    assert_eq!(stats.incomplete, 0);

    let committed: Vec<_> = recovered
        .iter()
        .filter(|r| r.action == RecoveryAction::Commit)
        .collect();
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].entries.len(), 2);
}

#[tokio::test]
async fn multi_writer_concurrent_commits() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path());
    let (mut wal, factory) = Wal::open::<String, Vec<u8>>(config).unwrap();
    let factory = Arc::new(factory);

    let mut handles = Vec::new();
    for i in 0..4 {
        let f = Arc::clone(&factory);
        handles.push(tokio::spawn(async move {
            let writer = f.register().unwrap();
            for j in 0..10 {
                let mut tx = Transaction::new();
                tx.insert(format!("w{i}-k{j}"), format!("v{i}-{j}").into_bytes());
                tx.commit(&writer).await.unwrap();
            }
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    wal.shutdown().await.unwrap();

    let (recovered, stats) = recover::<String, Vec<u8>>(tmp.path()).unwrap();
    assert_eq!(stats.committed, 40); // 4 writers × 10 txns
    assert_eq!(stats.incomplete, 0);

    let total_entries: usize = recovered
        .iter()
        .filter(|r| r.action == RecoveryAction::Commit)
        .map(|r| r.entries.len())
        .sum();
    assert_eq!(total_entries, 40); // 1 insert per txn
}

#[tokio::test]
async fn abort_discards_entries() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path());
    let (mut wal, factory) = Wal::open::<String, Vec<u8>>(config).unwrap();
    let writer = factory.register().unwrap();

    // Committed transaction
    let mut tx1 = Transaction::new();
    tx1.insert("kept".into(), b"yes".to_vec());
    tx1.commit(&writer).await.unwrap();

    // Aborted transaction
    let mut tx2 = Transaction::new();
    tx2.insert("discarded".into(), b"no".to_vec());
    tx2.abort(&writer).await.unwrap();

    wal.shutdown().await.unwrap();

    let (recovered, stats) = recover::<String, Vec<u8>>(tmp.path()).unwrap();
    assert_eq!(stats.committed, 1);
    assert_eq!(stats.aborted, 1);

    let committed_keys: Vec<String> = recovered
        .iter()
        .filter(|r| r.action == RecoveryAction::Commit)
        .flat_map(|r| r.entries.iter())
        .filter_map(|e| match e {
            WalEntry::Insert { key, .. } => Some(key.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(committed_keys, vec!["kept".to_string()]);
}

#[tokio::test]
async fn checkpoint_write_read() {
    let tmp = TempDir::new().unwrap();

    write_checkpoint(tmp.path(), 42).unwrap();
    let lsn = read_checkpoint(tmp.path()).unwrap();
    assert_eq!(lsn, 42);
}

#[tokio::test]
async fn checkpoint_missing_returns_zero() {
    let tmp = TempDir::new().unwrap();
    let lsn = read_checkpoint(tmp.path()).unwrap();
    assert_eq!(lsn, 0);
}

#[tokio::test]
async fn segment_rotation() {
    let tmp = TempDir::new().unwrap();
    let config = WalConfig::new(tmp.path())
        .with_ring_bits(10)
        .with_max_writers(2)
        .with_max_segment_size(4096) // Small — forces rotation
        .with_flush_interval(std::time::Duration::from_millis(5))
        .with_batch_hint(32);

    let (mut wal, factory) = Wal::open::<String, Vec<u8>>(config).unwrap();
    let writer = factory.register().unwrap();

    // Write enough data to trigger segment rotation (each entry ~530 bytes)
    for i in 0..50 {
        let mut tx = Transaction::new();
        tx.insert(format!("key{i}"), vec![0u8; 512]);
        tx.commit(&writer).await.unwrap();
    }

    wal.shutdown().await.unwrap();

    // Verify multiple segment files were created
    let segment_files: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("wal-")
        })
        .collect();
    assert!(
        segment_files.len() > 1,
        "Expected multiple segments, got {}",
        segment_files.len()
    );

    // Recovery should still find all committed transactions
    let (_, stats) = recover::<String, Vec<u8>>(tmp.path()).unwrap();
    assert_eq!(stats.committed, 50);
}

#[tokio::test]
async fn backpressure_no_data_loss() {
    let tmp = TempDir::new().unwrap();
    let config = WalConfig::new(tmp.path())
        .with_ring_bits(6) // Only 64 slots — forces backpressure
        .with_max_writers(4)
        .with_max_segment_size(64 * 1024 * 1024)
        .with_flush_interval(std::time::Duration::from_millis(20))
        .with_batch_hint(16);

    let (mut wal, factory) = Wal::open::<String, Vec<u8>>(config).unwrap();
    let factory = Arc::new(factory);

    let mut handles = Vec::new();
    for i in 0..4 {
        let f = Arc::clone(&factory);
        handles.push(tokio::spawn(async move {
            let writer = f.register().unwrap();
            for j in 0..100 {
                let mut tx = Transaction::new();
                tx.insert(format!("bp-w{i}-k{j}"), vec![i as u8; 32]);
                tx.commit(&writer).await.unwrap();
            }
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    wal.shutdown().await.unwrap();

    let (_, stats) = recover::<String, Vec<u8>>(tmp.path()).unwrap();
    assert_eq!(stats.committed, 400); // 4 × 100, no loss
    assert_eq!(stats.incomplete, 0);
}

#[tokio::test]
async fn recovery_handles_empty_dir() {
    let tmp = TempDir::new().unwrap();
    let (recovered, stats) = recover::<String, Vec<u8>>(tmp.path()).unwrap();
    assert!(recovered.is_empty());
    assert_eq!(stats.total_transactions, 0);
}

#[tokio::test]
async fn writer_factory_max_writers() {
    let tmp = TempDir::new().unwrap();
    let config = WalConfig::new(tmp.path())
        .with_ring_bits(8)
        .with_max_writers(2);

    let (mut wal, factory) = Wal::open::<String, Vec<u8>>(config).unwrap();
    let _w1 = factory.register().unwrap();
    let _w2 = factory.register().unwrap();
    // Third registration should fail
    assert!(factory.register().is_err());

    wal.shutdown().await.unwrap();
}

#[tokio::test]
async fn checkpoint_advancement() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path());
    let (mut wal, factory) = Wal::open::<String, Vec<u8>>(config).unwrap();
    let writer = factory.register().unwrap();

    // Write two transactions
    for i in 0..2 {
        let mut tx = Transaction::new();
        tx.insert(format!("k{i}"), format!("v{i}").into_bytes());
        tx.commit(&writer).await.unwrap();
    }
    wal.shutdown().await.unwrap();

    // Checkpoint should advance to highest committed tx_id
    let lsn = wal.checkpoint::<String, Vec<u8>>().unwrap();
    assert!(lsn > 0, "checkpoint LSN should advance");

    // Second checkpoint should return NoNewCheckpoints
    let result = wal.checkpoint::<String, Vec<u8>>();
    assert!(
        result.is_err(),
        "second checkpoint with no new writes should be NoNewCheckpoints"
    );
}

#[tokio::test]
async fn checkpoint_scheduler_truncates_segments() {
    let tmp = TempDir::new().unwrap();
    // Small segments to force rotation
    let config = WalConfig::new(tmp.path())
        .with_ring_bits(10)
        .with_max_writers(4)
        .with_max_segment_size(4096) // 4KB segments — forces many rotations
        .with_flush_interval(std::time::Duration::from_millis(5))
        .with_batch_hint(64);

    let (mut wal, factory) = Wal::open::<String, Vec<u8>>(config).unwrap();
    let writer = factory.register().unwrap();

    // Write enough data to generate multiple segments
    for i in 0..50 {
        let mut tx = Transaction::new();
        tx.insert(format!("key-{i}"), vec![0u8; 512]);
        tx.commit(&writer).await.unwrap();
    }

    // Start the checkpoint scheduler with a short interval
    wal.start_checkpoint_scheduler::<String, Vec<u8>>(std::time::Duration::from_millis(50));

    // Give the scheduler time to run at least once
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    wal.shutdown().await.unwrap();

    // Verify checkpoint file exists and is non-zero
    let lsn = read_checkpoint(tmp.path()).unwrap();
    assert!(lsn > 0, "scheduler should have advanced the checkpoint");
}

#[tokio::test]
async fn recover_into_store_replays_committed() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path());
    let (mut wal, factory) = Wal::open::<String, Vec<u8>>(config).unwrap();
    let writer = factory.register().unwrap();

    // Committed transaction
    let mut tx = Transaction::new();
    tx.insert("alive".to_string(), b"yes".to_vec());
    tx.commit(&writer).await.unwrap();

    // Aborted transaction
    let mut tx2 = Transaction::new();
    tx2.insert("dead".to_string(), b"no".to_vec());
    tx2.abort(&writer).await.unwrap();

    wal.shutdown().await.unwrap();

    // Recover into InMemoryStore
    let mut store = InMemoryStore::<String, Vec<u8>>::new();
    let stats =
        recover_into_store::<String, Vec<u8>, _>(tmp.path(), &mut store).unwrap();

    assert_eq!(stats.committed, 1);
    assert_eq!(stats.aborted, 1);
    assert_eq!(store.len(), 1);
    assert_eq!(store.get(&"alive".to_string()), Some(b"yes".to_vec()));
    assert!(store.get(&"dead".to_string()).is_none());
}

// ── Pipelined fsync tests ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pipelined_multi_writer_commits() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path())
        .with_sync_mode(SyncMode::Pipelined);

    let (mut wal, factory) = Wal::open::<String, Vec<u8>>(config).unwrap();

    let mut handles = Vec::new();
    for w in 0..4u8 {
        let writer = factory.register().unwrap();
        handles.push(tokio::spawn(async move {
            for i in 0..50u64 {
                let mut tx = Transaction::new();
                tx.insert(format!("k-{w}-{i}"), vec![w; 16]);
                tx.commit(&writer).await.unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    wal.shutdown().await.unwrap();

    // Recover and verify all 200 committed transactions
    let mut store = InMemoryStore::<String, Vec<u8>>::new();
    let stats =
        recover_into_store::<String, Vec<u8>, _>(tmp.path(), &mut store).unwrap();

    assert_eq!(stats.committed, 200);
    assert_eq!(stats.aborted, 0);
    assert_eq!(store.len(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pipelined_single_writer_commit() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path())
        .with_sync_mode(SyncMode::Pipelined);

    let (mut wal, factory) = Wal::open::<String, Vec<u8>>(config).unwrap();
    let writer = factory.register().unwrap();

    let mut tx = Transaction::new();
    tx.insert("key1".into(), b"value1".to_vec());
    tx.insert("key2".into(), b"value2".to_vec());
    tx.commit(&writer).await.unwrap();

    wal.shutdown().await.unwrap();

    let (recovered, stats) = recover::<String, Vec<u8>>(tmp.path()).unwrap();
    assert_eq!(stats.committed, 1);
    let committed: Vec<_> = recovered
        .iter()
        .filter(|r| r.action == RecoveryAction::Commit)
        .collect();
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].entries.len(), 2);
}
