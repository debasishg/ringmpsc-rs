//! Optional transaction wrapper for atomic multi-operation commits.
//!
//! Buffers operations locally and flushes them to the ring atomically
//! on `commit()`. This matches `async-wal-db`'s `Transaction` pattern.

use serde::Serialize;

use crate::entry::WalEntry;
use crate::error::WalError;
use crate::writer::{next_tx_id, WalWriter};

/// Transaction state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxState {
    Active,
    Committed,
    Aborted,
}

/// A transaction that buffers WAL operations and flushes atomically on commit.
///
/// Operations are held in a local `Vec` until `commit()` is called, at which
/// point they are sent through the writer's SPSC ring buffer followed by a
/// `Commit` marker. The `commit()` call awaits durability (fsync).
pub struct Transaction<K, V> {
    pub id: u64,
    pub state: TxState,
    entries: Vec<WalEntry<K, V>>,
}

impl<K, V> Default for Transaction<K, V>
where
    K: Serialize + Send + 'static,
    V: Serialize + Send + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> Transaction<K, V>
where
    K: Serialize + Send + 'static,
    V: Serialize + Send + 'static,
{
    /// Creates a new active transaction with a unique ID.
    #[must_use] 
    pub fn new() -> Self {
        Self {
            id: next_tx_id(),
            state: TxState::Active,
            entries: Vec::new(),
        }
    }

    /// Buffers an insert operation.
    pub fn insert(&mut self, key: K, value: V) {
        self.entries.push(WalEntry::Insert {
            tx_id: self.id,
            timestamp: WalEntry::<K, V>::new_timestamp(),
            key,
            value,
        });
    }

    /// Buffers an update operation.
    pub fn update(&mut self, key: K, old_value: V, new_value: V) {
        self.entries.push(WalEntry::Update {
            tx_id: self.id,
            timestamp: WalEntry::<K, V>::new_timestamp(),
            key,
            old_value,
            new_value,
        });
    }

    /// Buffers a delete operation.
    pub fn delete(&mut self, key: K, old_value: V) {
        self.entries.push(WalEntry::Delete {
            tx_id: self.id,
            timestamp: WalEntry::<K, V>::new_timestamp(),
            key,
            old_value,
        });
    }

    /// Flushes all buffered entries to the WAL and sends a commit marker.
    ///
    /// Awaits until the flusher has fsynced the batch containing the commit
    /// marker (group commit). Returns `Ok(())` once durability is guaranteed.
    pub async fn commit(mut self, writer: &WalWriter<K, V>) -> Result<(), WalError> {
        if self.state != TxState::Active {
            return Err(WalError::AlreadyFinalized { tx_id: self.id });
        }

        // Send all buffered entries
        for entry in self.entries.drain(..) {
            writer.append(entry).await?;
        }

        // Send commit marker and wait for durability
        writer.commit(self.id).await?;
        self.state = TxState::Committed;
        Ok(())
    }

    /// Sends an abort marker. Buffered entries are discarded.
    pub async fn abort(mut self, writer: &WalWriter<K, V>) -> Result<(), WalError> {
        if self.state != TxState::Active {
            return Err(WalError::AlreadyFinalized { tx_id: self.id });
        }

        self.entries.clear();
        writer.abort(self.id).await?;
        self.state = TxState::Aborted;
        Ok(())
    }

    /// Returns the number of buffered (unsent) entries.
    #[must_use] 
    pub fn pending_count(&self) -> usize {
        self.entries.len()
    }

    /// Returns the current transaction state.
    #[must_use] 
    pub fn state(&self) -> TxState {
        self.state
    }
}
