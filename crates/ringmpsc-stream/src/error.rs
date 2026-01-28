//! Error types for ringmpsc-stream operations.

use ringmpsc_rs::ChannelError;
use thiserror::Error;

/// Errors that can occur in stream operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum StreamError {
    /// The ring buffer is full and cannot accept more items.
    #[error("ring buffer is full")]
    Full,

    /// The channel has been closed.
    #[error("channel is closed")]
    Closed,

    /// Failed to register a new producer.
    #[error("registration failed: {0}")]
    RegistrationFailed(#[from] ChannelError),

    /// The stream has been shut down.
    #[error("stream has been shut down")]
    ShutDown,
}

impl StreamError {
    /// Returns `true` if this is a recoverable error (e.g., `Full`).
    #[inline]
    pub fn is_recoverable(&self) -> bool {
        matches!(self, Self::Full)
    }

    /// Returns `true` if this error indicates the channel is permanently unusable.
    #[inline]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Closed | Self::ShutDown)
    }
}
