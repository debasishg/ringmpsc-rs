//! WAL configuration.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Configuration for the ring-buffer WAL.
#[derive(Debug, Clone)]
pub struct WalConfig {
    /// Directory for WAL segment files.
    pub dir: PathBuf,
    /// Power-of-2 ring buffer capacity per writer: `2^ring_bits` slots.
    /// Default: 14 (16,384 slots).
    pub ring_bits: u8,
    /// Maximum number of concurrent writers.
    /// Default: 16.
    pub max_writers: usize,
    /// Maximum segment file size in bytes before rotation.
    /// Default: 64 MB.
    pub max_segment_size: u64,
    /// Background flusher poll interval.
    /// Default: 10ms.
    pub flush_interval: Duration,
    /// Hint for batch drain size per flush cycle.
    /// Default: 256.
    pub batch_hint: usize,
    /// Enable per-ring metrics collection.
    /// Default: false.
    pub enable_metrics: bool,
}

impl WalConfig {
    /// Creates a new config for the given WAL directory.
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().to_path_buf(),
            ring_bits: 14,
            max_writers: 16,
            max_segment_size: 64 * 1024 * 1024,
            flush_interval: Duration::from_millis(10),
            batch_hint: 256,
            enable_metrics: false,
        }
    }

    pub fn with_ring_bits(mut self, bits: u8) -> Self {
        assert!((1..=20).contains(&bits), "ring_bits must be 1..=20");
        self.ring_bits = bits;
        self
    }

    pub fn with_max_writers(mut self, n: usize) -> Self {
        assert!(n >= 1, "max_writers must be >= 1");
        self.max_writers = n;
        self
    }

    pub fn with_max_segment_size(mut self, bytes: u64) -> Self {
        assert!(bytes >= 4096, "max_segment_size must be >= 4096");
        self.max_segment_size = bytes;
        self
    }

    pub fn with_flush_interval(mut self, d: Duration) -> Self {
        self.flush_interval = d;
        self
    }

    pub fn with_batch_hint(mut self, n: usize) -> Self {
        self.batch_hint = n;
        self
    }

    pub fn with_metrics(mut self, enable: bool) -> Self {
        self.enable_metrics = enable;
        self
    }

    /// Returns the ring capacity per writer.
    pub fn ring_capacity(&self) -> usize {
        1 << self.ring_bits
    }
}
