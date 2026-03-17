//! WAL configuration.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Controls when and how the flusher syncs data to disk after writing a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// `sync_all()` after every batch — full durability (default).
    /// Blocks the flusher task during fsync.
    Full,
    /// `sync_data()` after every batch — flushes file data but not metadata
    /// (e.g. file size, mtime). Faster than `Full` on Linux/ext4 where
    /// metadata sync is expensive. On macOS this is equivalent to `Full`.
    DataOnly,
    /// Flush + `spawn_blocking(sync_all)` — offloads fsync to Tokio's
    /// blocking thread pool. The flusher awaits the result but yields
    /// the worker thread so writers on other threads keep filling rings.
    /// Requires `multi_thread` runtime for any benefit.
    Background,
    /// **Pipelined**: fire-and-forget the *previous* batch's fsync while
    /// draining/writing the *next* batch. Strict durability is preserved
    /// (commit waiters are only notified after *their* batch has fully
    /// synced). This is the performance unlock over `Background`.
    /// Requires `multi_thread` runtime.
    Pipelined,
    /// **`PipelinedDataOnly`**: same as `Pipelined` but uses `sync_data()`
    /// (fdatasync) instead of `sync_all()`. Skips metadata updates
    /// (atime/mtime/size) — potentially 30–80% faster on Linux/ext4
    /// for append-mostly workloads. On macOS APFS this is equivalent
    /// to `Pipelined`. Requires `multi_thread` runtime.
    PipelinedDataOnly,
    /// **`PipelinedDedicated`**: same fire-and-forget overlap as `Pipelined`
    /// but uses a dedicated OS thread + bounded channel instead of Tokio's
    /// `spawn_blocking` pool. Lower tail latency at high writer counts.
    /// Requires `multi_thread` runtime.
    PipelinedDedicated,
    /// Flush `BufWriter` to kernel buffer only — faster but data may be
    /// lost on crash. Useful for benchmarks and testing.
    None,
}

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
    /// Sync mode: `Full` | `DataOnly` | `Background` | `Pipelined` |
    /// `PipelinedDataOnly` | `PipelinedDedicated` | `None`.
    /// Default: `SyncMode::Full`.
    pub sync_mode: SyncMode,
    /// Enable direct I/O (bypass page cache) on segment files.
    ///
    /// On macOS this sets `F_NOCACHE` via `fcntl()`, which avoids polluting
    /// the page cache with append-only WAL data. On Linux this would use
    /// `O_DIRECT` (not yet implemented — requires aligned writes).
    ///
    /// Best combined with `SyncMode::DataOnly` or `SyncMode::PipelinedDataOnly`
    /// and 4 KiB+ payloads for optimal fsync latency on Apple Silicon.
    ///
    /// Default: `false`.
    pub direct_io: bool,
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
            sync_mode: SyncMode::Full,
            direct_io: false,
        }
    }

    #[must_use] 
    pub fn with_ring_bits(mut self, bits: u8) -> Self {
        assert!((1..=20).contains(&bits), "ring_bits must be 1..=20");
        self.ring_bits = bits;
        self
    }

    #[must_use] 
    pub fn with_max_writers(mut self, n: usize) -> Self {
        assert!(n >= 1, "max_writers must be >= 1");
        self.max_writers = n;
        self
    }

    #[must_use] 
    pub fn with_max_segment_size(mut self, bytes: u64) -> Self {
        assert!(bytes >= 4096, "max_segment_size must be >= 4096");
        self.max_segment_size = bytes;
        self
    }

    #[must_use] 
    pub fn with_flush_interval(mut self, d: Duration) -> Self {
        self.flush_interval = d;
        self
    }

    #[must_use] 
    pub fn with_batch_hint(mut self, n: usize) -> Self {
        self.batch_hint = n;
        self
    }

    #[must_use] 
    pub fn with_metrics(mut self, enable: bool) -> Self {
        self.enable_metrics = enable;
        self
    }

    #[must_use] 
    pub fn with_sync_mode(mut self, mode: SyncMode) -> Self {
        self.sync_mode = mode;
        self
    }

    #[must_use] 
    pub fn with_direct_io(mut self, enable: bool) -> Self {
        self.direct_io = enable;
        self
    }

    /// Returns the ring capacity per writer.
    #[must_use] 
    pub fn ring_capacity(&self) -> usize {
        1 << self.ring_bits
    }
}
