//! Storage backend trait and in-memory implementation.
//!
//! `WalStore` defines the contract for applying WAL entries to a storage engine.
//! After crash recovery, committed transactions are replayed through this trait
//! to rebuild application state.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, RwLock};

use ringwal::{
    recover, IoEngine, RecoveredTransaction, RecoveryAction, RecoveryStats, WalEntry, WalError,
};

/// A storage backend that WAL entries can be applied to.
///
/// Implement this trait for your storage engine (`HashMap`, LMDB, `RocksDB`, etc.)
/// to enable automatic replay of committed transactions during recovery.
///
/// # Type Parameters
/// - `K`: Key type (must match the WAL's key type)
/// - `V`: Value type (must match the WAL's value type)
pub trait WalStore<K, V> {
    /// Apply a single WAL entry to the store.
    ///
    /// Only called for entries from committed transactions.
    /// `Commit` and `Abort` marker entries are never passed to this method.
    fn apply(&mut self, entry: &WalEntry<K, V>) -> Result<(), WalError>;
}

/// In-memory key-value store backed by `HashMap`.
///
/// Thread-safe via `Arc<RwLock<...>>`. Equivalent to async-wal-db's
/// `Arc<RwLock<HashMap<String, Vec<u8>>>>` but generic over `K` and `V`.
///
/// # Example
///
/// ```ignore
/// use ringwal::RealIo;
/// use ringwal_store::{InMemoryStore, recover_into_store};
///
/// let mut store = InMemoryStore::<String, Vec<u8>>::new();
/// let stats = recover_into_store(dir, &mut store, &RealIo)?;
/// let snapshot = store.snapshot();
/// ```
pub struct InMemoryStore<K, V> {
    inner: Arc<RwLock<HashMap<K, V>>>,
}

impl<K, V> InMemoryStore<K, V>
where
    K: Eq + Hash,
{
    /// Creates an empty in-memory store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Returns a clone of the current store contents.
    #[must_use]
    pub fn snapshot(&self) -> HashMap<K, V>
    where
        K: Clone,
        V: Clone,
    {
        self.inner.read().expect("lock poisoned").clone()
    }

    /// Returns the number of entries in the store.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.read().expect("lock poisoned").len()
    }

    /// Returns `true` if the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.read().expect("lock poisoned").is_empty()
    }

    /// Gets a value by key (cloned).
    pub fn get(&self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        self.inner.read().expect("lock poisoned").get(key).cloned()
    }
}

impl<K, V> Default for InMemoryStore<K, V>
where
    K: Eq + Hash,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> Clone for InMemoryStore<K, V> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<K, V> WalStore<K, V> for InMemoryStore<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    fn apply(&mut self, entry: &WalEntry<K, V>) -> Result<(), WalError> {
        let mut map = self.inner.write().expect("lock poisoned");
        match entry {
            WalEntry::Insert { key, value, .. } => {
                map.insert(key.clone(), value.clone());
            }
            WalEntry::Update { key, new_value, .. } => {
                map.insert(key.clone(), new_value.clone());
            }
            WalEntry::Delete { key, .. } => {
                map.remove(key);
            }
            WalEntry::Commit { .. } | WalEntry::Abort { .. } => {
                // Markers — nothing to apply
            }
        }
        Ok(())
    }
}

/// Recovers committed transactions from WAL segments and applies them to a store.
///
/// This is the main entry point for crash recovery with a storage backend.
/// Only committed transactions are replayed; aborted and incomplete are skipped.
///
/// Returns the recovery statistics.
pub fn recover_into_store<K, V, S, IO>(
    dir: &std::path::Path,
    store: &mut S,
    io: &IO,
) -> Result<RecoveryStats, WalError>
where
    K: serde::de::DeserializeOwned + Send + 'static,
    V: serde::de::DeserializeOwned + Send + 'static,
    S: WalStore<K, V>,
    IO: IoEngine,
{
    let (transactions, stats) = recover::<K, V, IO>(dir, io)?;
    apply_transactions(store, &transactions)?;
    Ok(stats)
}

/// Applies a set of recovered transactions to a store.
///
/// Only committed transactions are applied.
pub fn apply_transactions<K, V, S>(
    store: &mut S,
    transactions: &[RecoveredTransaction<K, V>],
) -> Result<(), WalError>
where
    S: WalStore<K, V>,
{
    for tx in transactions {
        if tx.action == RecoveryAction::Commit {
            for entry in &tx.entries {
                store.apply(entry)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_store_insert_get() {
        let mut store = InMemoryStore::<String, Vec<u8>>::new();
        let entry = WalEntry::Insert {
            tx_id: 1,
            timestamp: 0,
            key: "k1".to_string(),
            value: b"v1".to_vec(),
        };
        store.apply(&entry).unwrap();
        assert_eq!(store.get(&"k1".to_string()), Some(b"v1".to_vec()));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn in_memory_store_update() {
        let mut store = InMemoryStore::<String, Vec<u8>>::new();
        store
            .apply(&WalEntry::Insert {
                tx_id: 1,
                timestamp: 0,
                key: "k1".to_string(),
                value: b"old".to_vec(),
            })
            .unwrap();
        store
            .apply(&WalEntry::Update {
                tx_id: 1,
                timestamp: 0,
                key: "k1".to_string(),
                old_value: b"old".to_vec(),
                new_value: b"new".to_vec(),
            })
            .unwrap();
        assert_eq!(store.get(&"k1".to_string()), Some(b"new".to_vec()));
    }

    #[test]
    fn in_memory_store_delete() {
        let mut store = InMemoryStore::<String, Vec<u8>>::new();
        store
            .apply(&WalEntry::Insert {
                tx_id: 1,
                timestamp: 0,
                key: "k1".to_string(),
                value: b"v1".to_vec(),
            })
            .unwrap();
        store
            .apply(&WalEntry::Delete {
                tx_id: 1,
                timestamp: 0,
                key: "k1".to_string(),
                old_value: b"v1".to_vec(),
            })
            .unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn apply_transactions_only_committed() {
        let mut store = InMemoryStore::<String, Vec<u8>>::new();
        let transactions = vec![
            RecoveredTransaction {
                tx_id: 1,
                action: RecoveryAction::Commit,
                entries: vec![WalEntry::Insert {
                    tx_id: 1,
                    timestamp: 0,
                    key: "committed".to_string(),
                    value: b"yes".to_vec(),
                }],
            },
            RecoveredTransaction {
                tx_id: 2,
                action: RecoveryAction::Rollback,
                entries: vec![WalEntry::Insert {
                    tx_id: 2,
                    timestamp: 0,
                    key: "aborted".to_string(),
                    value: b"no".to_vec(),
                }],
            },
            RecoveredTransaction {
                tx_id: 3,
                action: RecoveryAction::Incomplete,
                entries: vec![WalEntry::Insert {
                    tx_id: 3,
                    timestamp: 0,
                    key: "incomplete".to_string(),
                    value: b"no".to_vec(),
                }],
            },
        ];
        apply_transactions(&mut store, &transactions).unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.get(&"committed".to_string()),
            Some(b"yes".to_vec())
        );
        assert!(store.get(&"aborted".to_string()).is_none());
        assert!(store.get(&"incomplete".to_string()).is_none());
    }
}
