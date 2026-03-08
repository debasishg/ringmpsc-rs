//! WAL error types.

use thiserror::Error;

/// Errors that can occur during WAL operations.
#[derive(Error, Debug)]
pub enum WalError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] bincode::Error),

    #[error("Checksum mismatch: expected {expected:#x}, got {actual:#x}")]
    ChecksumMismatch { expected: u32, actual: u32 },

    #[error("Partial write detected at offset {offset}")]
    PartialWrite { offset: u64 },

    #[error("Segment full (size {size} >= max {max_size})")]
    SegmentFull { size: u64, max_size: u64 },

    #[error("WAL closed")]
    Closed,

    #[error("Transaction {tx_id} already finalized")]
    AlreadyFinalized { tx_id: u64 },

    #[error("Max writers reached ({max})")]
    MaxWriters { max: usize },

    #[error("Invalid segment file: {0}")]
    InvalidSegment(String),

    #[error("No new checkpoints available")]
    NoNewCheckpoints,
}
