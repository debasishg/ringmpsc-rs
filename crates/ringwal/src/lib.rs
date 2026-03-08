//! Ring-buffer Write-Ahead Log (`ringwal`)
//!
//! A WAL backed by multiple SPSC ring buffers via `ringmpsc-rs`, providing
//! lock-free parallel writes with zero producer-producer contention.
//!
//! # Architecture
//!
//! Each writer gets a dedicated SPSC ring buffer. A single background flusher
//! task drains all rings, serializes entries with CRC32 checksums, writes to
//! rotatable segment files, and fsyncs for durability. Commit waiters are
//! notified via group commit — one fsync unblocks all commits in the batch.
//!
//! # Example
//!
//! ```ignore
//! use ringwal::{Wal, WalConfig, Transaction};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), ringwal::WalError> {
//!     let config = WalConfig::new("/tmp/my_wal");
//!     let (mut wal, factory) = Wal::open::<String, Vec<u8>>(config)?;
//!
//!     let writer = factory.register()?;
//!
//!     // Transactional write
//!     let mut tx = Transaction::new();
//!     tx.insert("key1".into(), b"value1".to_vec());
//!     tx.insert("key2".into(), b"value2".to_vec());
//!     tx.commit(&writer).await?;
//!
//!     wal.shutdown().await?;
//!     Ok(())
//! }
//! ```

mod config;
mod entry;
mod error;
mod invariants;
mod recovery;
mod segment;
mod transaction;
mod wal;
mod writer;

pub use config::WalConfig;
pub use entry::{ByteWalEntry, WalEntry, WalEntryHeader};
pub use error::WalError;
pub use recovery::{
    read_checkpoint, recover, write_checkpoint, RecoveredTransaction, RecoveryAction,
    RecoveryStats,
};
pub use segment::{SegmentMeta, SegmentManager};
pub use transaction::{Transaction, TxState};
pub use wal::Wal;
pub use writer::{next_tx_id, WalWriter, WalWriterFactory};
