//! Integration tests for recovering a ringwal WAL into a store.

use ringwal::{RealIo, Transaction, Wal, WalConfig};
use ringwal_store::{recover_into_store, InMemoryStore};
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
async fn recover_into_store_replays_committed() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path());
    let (mut wal, factory) = Wal::open::<String, Vec<u8>>(config, RealIo).unwrap();
    let mut writer = factory.register().unwrap();

    // Committed transaction
    let mut tx = Transaction::new();
    tx.insert("alive".to_string(), b"yes".to_vec());
    tx.commit(&mut writer).await.unwrap();

    // Aborted transaction
    let mut tx2 = Transaction::new();
    tx2.insert("dead".to_string(), b"no".to_vec());
    tx2.abort(&mut writer).await.unwrap();

    wal.shutdown().await.unwrap();

    // Recover into InMemoryStore
    let mut store = InMemoryStore::<String, Vec<u8>>::new();
    let stats =
        recover_into_store::<String, Vec<u8>, _, _>(tmp.path(), &mut store, &RealIo).unwrap();

    assert_eq!(stats.committed, 1);
    assert_eq!(stats.aborted, 1);
    assert_eq!(store.len(), 1);
    assert_eq!(store.get(&"alive".to_string()), Some(b"yes".to_vec()));
    assert!(store.get(&"dead".to_string()).is_none());
}
