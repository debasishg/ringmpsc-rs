//! Integration tests for ringwal.

use ringwal::{
    recover, read_checkpoint, write_checkpoint, RecoveryAction, Transaction, Wal, WalConfig,
    WalEntry,
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
