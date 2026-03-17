//! I/O abstraction layer for deterministic simulation testing.
//!
//! All WAL file operations go through the [`IoEngine`] trait, which has two
//! implementations:
//!
//! - [`RealIo`] — zero-overhead wrapper around `std::fs` for production.
//! - `SimIo` — in-memory filesystem with fault injection (in the `ringwal-sim` crate).
//!
//! Production code uses `IO = RealIo` via type aliases, so the generic parameter
//! is invisible to callers unless they need a custom backend.

mod real;

pub use real::RealIo;

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// A directory entry returned by [`IoEngine::read_dir`].
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub file_name: String,
    pub path: PathBuf,
}

/// Top-level I/O abstraction. Production: [`RealIo`]. Simulation: `SimIo`.
///
/// Aggregates file, directory, and clock operations. All methods are
/// synchronous — ringwal uses `std::fs`, not `tokio::fs`.
///
/// Requires `Clone` so the engine can be shared between the `Wal` handle,
/// `SegmentManager`, and background tasks (checkpoint scheduler). For
/// production `RealIo` this is `Copy`; simulation backends typically use
/// an `Arc`-based interior.
pub trait IoEngine: Send + Sync + Clone + 'static {
    /// Writable file handle (append mode).
    type FileHandle: FileHandle;
    /// Readable file handle.
    type ReadHandle: ReadHandle;

    /// Opens or creates a file for appending.
    ///
    /// When `direct_io` is `true`, applies platform-specific page-cache bypass
    /// (e.g. macOS `F_NOCACHE`).
    fn open_append(&self, path: &Path, direct_io: bool) -> io::Result<Self::FileHandle>;

    /// Opens a file for reading.
    fn open_read(&self, path: &Path) -> io::Result<Self::ReadHandle>;

    // ── Directory operations ─────────────────────────────────────────────

    /// Creates a directory and all parent directories.
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;

    /// Lists entries in a directory.
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>>;

    /// Removes a file. Returns `Ok(())` even if the file does not exist.
    fn remove_file(&self, path: &Path) -> io::Result<()>;

    /// Atomically writes `data` to a file (create or overwrite).
    ///
    /// Used for checkpoint files where partial writes must not be visible.
    fn write_file_bytes(&self, path: &Path, data: &[u8]) -> io::Result<()>;

    /// Reads the entire contents of a file.
    fn read_file_bytes(&self, path: &Path) -> io::Result<Vec<u8>>;

    /// Returns `true` if the path exists.
    fn exists(&self, path: &Path) -> bool;

    // ── Clock ────────────────────────────────────────────────────────────

    /// Returns the current time as seconds since the UNIX epoch.
    fn now_secs(&self) -> u64;
}

/// A writable file handle returned by [`IoEngine::open_append`].
///
/// Extends `std::io::Write` with sync and metadata operations.
pub trait FileHandle: Write + Send + 'static {
    /// Flushes application buffers and syncs data + metadata to durable storage.
    fn sync_all(&mut self) -> io::Result<()>;

    /// Flushes application buffers and syncs data only (no metadata) to durable storage.
    ///
    /// Maps to `fdatasync` on Linux, `F_FULLFSYNC` on macOS.
    fn sync_data(&mut self) -> io::Result<()>;

    /// Duplicates the underlying file descriptor.
    ///
    /// The clone shares the same OS file descriptor, so `sync_all()` on
    /// the clone flushes the same inode. Used for pipelined fsync.
    fn try_clone_file(&self) -> io::Result<std::fs::File>;

    /// Returns the current file size in bytes.
    fn metadata_len(&self) -> io::Result<u64>;
}

/// A readable file handle returned by [`IoEngine::open_read`].
///
/// Extends `std::io::Read` with metadata access.
pub trait ReadHandle: Read + Send + 'static {
    /// Returns the file size in bytes.
    fn metadata_len(&self) -> io::Result<u64>;
}
