//! WAL writer — one per producer, wraps a `RingSender`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::oneshot;

use crate::entry::WalEntry;
use crate::error::WalError;
use crate::wal::CommitRegistry;
use ringmpsc_stream::{RingSender, SenderFactory, StreamError};

/// Internal envelope sent through the ring buffer.
///
/// Wraps a `WalEntry` with an optional commit-notification channel used
/// by the flusher to signal durability back to the writer.
pub(crate) enum Envelope<K, V> {
    Entry(WalEntry<K, V>),
    /// A commit entry bundled with a oneshot sender that the flusher
    /// will fire after the batch containing this commit is fsynced.
    CommitBarrier {
        entry: WalEntry<K, V>,
        tx: oneshot::Sender<()>,
    },
}

/// Global monotonic transaction ID generator.
static NEXT_TX_ID: AtomicU64 = AtomicU64::new(1);

/// Allocates a new unique transaction ID.
pub fn next_tx_id() -> u64 {
    NEXT_TX_ID.fetch_add(1, Ordering::Relaxed)
}

/// Resets the tx ID counter (used during recovery to avoid reuse).
pub(crate) fn reset_tx_id(min: u64) {
    NEXT_TX_ID.fetch_max(min, Ordering::Relaxed);
}

/// Factory for creating [`WalWriter`] instances.
///
/// Each call to `register()` allocates a dedicated SPSC ring buffer for
/// the writer, ensuring zero contention with other writers.
pub struct WalWriterFactory<K, V> {
    factory: Arc<SenderFactory<Envelope<K, V>>>,
    commit_registry: Arc<CommitRegistry>,
}

impl<K, V> WalWriterFactory<K, V>
where
    K: Serialize + Send + 'static,
    V: Serialize + Send + 'static,
{
    pub(crate) fn new(
        factory: SenderFactory<Envelope<K, V>>,
        commit_registry: Arc<CommitRegistry>,
    ) -> Self {
        Self {
            factory: Arc::new(factory),
            commit_registry,
        }
    }

    /// Registers a new writer backed by its own SPSC ring buffer.
    ///
    /// Returns `Err` if the maximum number of writers has been reached
    /// or the WAL is closed.
    pub fn register(&self) -> Result<WalWriter<K, V>, WalError> {
        let sender = self.factory.register().map_err(|e| match e {
            StreamError::Full => WalError::MaxWriters {
                max: 0, // actual max is in config
            },
            _ => WalError::Closed,
        })?;
        Ok(WalWriter {
            sender,
        })
    }

    /// Returns the number of registered writers.
    pub fn writer_count(&self) -> usize {
        self.factory.producer_count()
    }

    /// Closes the factory, preventing new writer registrations.
    pub fn close(&self) {
        self.factory.close();
    }
}

impl<K, V> Clone for WalWriterFactory<K, V> {
    fn clone(&self) -> Self {
        Self {
            factory: Arc::clone(&self.factory),
            commit_registry: Arc::clone(&self.commit_registry),
        }
    }
}

/// A WAL writer handle. Each writer has a dedicated SPSC ring buffer —
/// writes are lock-free with zero contention against other writers.
pub struct WalWriter<K, V> {
    sender: RingSender<Envelope<K, V>>,
}

impl<K, V> WalWriter<K, V>
where
    K: Serialize + Send + 'static,
    V: Serialize + Send + 'static,
{
    /// Appends an entry to the WAL.
    ///
    /// Applies backpressure (async wait) if the ring buffer is full.
    /// The entry is _not_ durable until a subsequent `commit()` completes.
    pub async fn append(&self, entry: WalEntry<K, V>) -> Result<(), WalError> {
        self.sender
            .send(Envelope::Entry(entry))
            .await
            .map_err(|_| WalError::Closed)
    }

    /// Sends a commit marker for `tx_id` and waits until the batch
    /// containing it has been fsynced to disk (group commit).
    ///
    /// Returns `Ok(())` once durability is guaranteed.
    pub async fn commit(&self, tx_id: u64) -> Result<(), WalError> {
        let (tx, rx) = oneshot::channel();
        let entry = WalEntry::Commit {
            tx_id,
            timestamp: WalEntry::<K, V>::new_timestamp(),
        };
        self.sender
            .send(Envelope::CommitBarrier { entry, tx })
            .await
            .map_err(|_| WalError::Closed)?;

        // Wait for flusher to fsync and notify us
        rx.await.map_err(|_| WalError::Closed)
    }

    /// Sends an abort marker for `tx_id`. Does not wait for durability.
    pub async fn abort(&self, tx_id: u64) -> Result<(), WalError> {
        let entry = WalEntry::Abort {
            tx_id,
            timestamp: WalEntry::<K, V>::new_timestamp(),
        };
        self.sender
            .send(Envelope::Entry(entry))
            .await
            .map_err(|_| WalError::Closed)
    }
}
