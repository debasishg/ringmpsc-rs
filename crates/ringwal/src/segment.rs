//! WAL segment files and rotation manager.
//!
//! Each segment is a sequential log file containing serialized WAL entries
//! with CRC32-checked headers. The `SegmentManager` handles rotation when
//! segments exceed the configured maximum size.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::entry::WalEntryHeader;
use crate::error::WalError;
use crate::invariants::{debug_assert_segment_id_monotonic, debug_assert_segment_size};
use crate::io::{IoEngine, FileHandle};

/// Metadata for a sealed (immutable) segment.
#[derive(Debug, Clone)]
pub struct SegmentMeta {
    pub id: u64,
    pub path: PathBuf,
    pub size: u64,
    pub entry_count: u64,
    pub first_lsn: u64,
    pub last_lsn: u64,
}

/// An active segment file being written to.
pub struct Segment<IO: IoEngine> {
    pub(crate) id: u64,
    pub(crate) path: PathBuf,
    pub(crate) file: IO::FileHandle,
    pub(crate) size: u64,
    pub(crate) max_size: u64,
    pub(crate) entry_count: u64,
    pub(crate) first_lsn: u64,
    pub(crate) last_lsn: u64,
}

impl<IO: IoEngine> Segment<IO> {
    /// Opens or creates a segment file via the given I/O engine.
    pub fn open(id: u64, dir: &Path, max_size: u64, direct_io: bool, io: &IO) -> Result<Self, WalError> {
        let path = segment_path(dir, id);
        let file = io.open_append(&path, direct_io)?;
        let size = file.metadata_len()?;

        Ok(Self {
            id,
            path,
            file,
            size,
            max_size,
            entry_count: 0,
            first_lsn: 0,
            last_lsn: 0,
        })
    }

    /// Appends a serialized entry (header + data) to this segment.
    ///
    /// Returns the number of bytes written (header + data).
    pub fn append_entry(&mut self, lsn: u64, data: &[u8]) -> Result<usize, WalError> {
        let header = WalEntryHeader::new(data);
        let header_bytes = header.to_bytes();
        let total = WalEntryHeader::SIZE + data.len();

        self.file.write_all(&header_bytes)?;
        self.file.write_all(data)?;

        self.size += total as u64;
        self.entry_count += 1;
        if self.first_lsn == 0 {
            self.first_lsn = lsn;
        }
        self.last_lsn = lsn;

        debug_assert_segment_size!(self.size, self.max_size + total as u64);

        Ok(total)
    }

    /// Returns `true` if the segment has reached or exceeded its max size.
    pub fn needs_rotation(&self) -> bool {
        self.size >= self.max_size
    }

    /// Flushes the buffer and fsyncs data + metadata to disk.
    pub fn fsync(&mut self) -> Result<(), WalError> {
        self.file.sync_all()?;
        Ok(())
    }

    /// Flushes the buffer and fsyncs data only (no metadata) to disk.
    ///
    /// Uses `sync_data()` which maps to `fdatasync` on Linux — avoids
    /// the metadata write that `sync_all()` requires. On macOS this is
    /// equivalent to `sync_all()` (`F_FULLFSYNC` flushes everything).
    pub fn fsync_data(&mut self) -> Result<(), WalError> {
        self.file.sync_data()?;
        Ok(())
    }

    /// Flushes the `BufWriter` buffer and returns a cloned file handle
    /// for background `sync_all()` via `spawn_blocking`.
    ///
    /// The clone shares the same underlying OS file descriptor (`dup()`),
    /// so `sync_all()` on the clone flushes the same inode.
    pub fn flush_and_clone_fd(&mut self) -> Result<std::fs::File, WalError> {
        self.file.flush()?;
        Ok(self.file.try_clone_file()?)
    }

    /// Seals this segment, returning its metadata. The file is fsynced and closed.
    pub fn seal(mut self) -> Result<SegmentMeta, WalError> {
        self.fsync()?;
        Ok(SegmentMeta {
            id: self.id,
            path: self.path,
            size: self.size,
            entry_count: self.entry_count,
            first_lsn: self.first_lsn,
            last_lsn: self.last_lsn,
        })
    }
}

/// Manages WAL segment creation, rotation, and cleanup.
pub struct SegmentManager<IO: IoEngine> {
    dir: PathBuf,
    max_segment_size: u64,
    direct_io: bool,
    active: Segment<IO>,
    sealed: Vec<SegmentMeta>,
    next_segment_id: u64,
    io: IO,
}

impl<IO: IoEngine> SegmentManager<IO> {
    /// Opens or creates a WAL directory and initialises the first segment.
    pub fn open(dir: &Path, max_segment_size: u64, direct_io: bool, io: IO) -> Result<Self, WalError> {
        io.create_dir_all(dir)?;

        // Find existing segments
        let mut segment_ids = discover_segment_ids(dir, &io)?;
        segment_ids.sort_unstable();

        let next_id = segment_ids.last().map_or(1, |&id| id + 1);
        let active = Segment::open(next_id, dir, max_segment_size, direct_io, &io)?;

        Ok(Self {
            dir: dir.to_path_buf(),
            max_segment_size,
            direct_io,
            active,
            sealed: Vec::new(),
            next_segment_id: next_id + 1,
            io,
        })
    }

    /// Writes a batch of pre-serialized entries to the active segment.
    ///
    /// Each entry in `batch` is `(lsn, serialized_data)`.
    /// Rotates the segment if it would exceed the size limit.
    pub fn write_batch(&mut self, batch: &[(u64, Vec<u8>)]) -> Result<(), WalError> {
        for (lsn, data) in batch {
            // Rotate before writing if current segment is full
            if self.active.needs_rotation() {
                self.rotate()?;
            }
            self.active.append_entry(*lsn, data)?;
        }
        Ok(())
    }

    /// Flushes and fsyncs the active segment (data + metadata).
    pub fn fsync(&mut self) -> Result<(), WalError> {
        self.active.fsync()
    }

    /// Flushes and fsyncs data only (no metadata) on the active segment.
    pub fn fsync_data(&mut self) -> Result<(), WalError> {
        self.active.fsync_data()
    }

    /// Flushes the active segment's `BufWriter` to the kernel buffer (no fsync).
    pub fn flush(&mut self) -> Result<(), WalError> {
        self.active.file.flush()?;
        Ok(())
    }

    /// Flushes the active segment's `BufWriter` and returns a cloned file
    /// handle for background `sync_all()`.
    pub fn flush_and_clone_fd(&mut self) -> Result<std::fs::File, WalError> {
        self.active.flush_and_clone_fd()
    }

    /// Rotates: seals the current segment and opens a new one.
    pub fn rotate(&mut self) -> Result<(), WalError> {
        let new_id = self.next_segment_id;
        debug_assert_segment_id_monotonic!(self.active.id, new_id);
        self.next_segment_id += 1;

        let new_segment = Segment::open(new_id, &self.dir, self.max_segment_size, self.direct_io, &self.io)?;
        let old_segment = std::mem::replace(&mut self.active, new_segment);
        let meta = old_segment.seal()?;
        self.sealed.push(meta);
        Ok(())
    }

    /// Returns metadata for all sealed segments.
    pub fn sealed_segments(&self) -> &[SegmentMeta] {
        &self.sealed
    }

    /// Returns the ID of the active segment.
    pub fn active_segment_id(&self) -> u64 {
        self.active.id
    }

    /// Removes sealed segments whose `last_lsn` is strictly less than the
    /// given checkpoint LSN.
    pub fn truncate_before(&mut self, checkpoint_lsn: u64) -> Result<usize, WalError> {
        let mut removed = 0;
        let io = &self.io;
        self.sealed.retain(|meta| {
            if meta.last_lsn < checkpoint_lsn {
                // Best-effort delete; ignore errors for already-removed files
                let _ = io.remove_file(&meta.path);
                removed += 1;
                false
            } else {
                true
            }
        });
        Ok(removed)
    }

    /// Returns the WAL directory path.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// Returns the file path for a segment with the given ID.
pub fn segment_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("wal-{id:08}.log"))
}

/// Discovers existing segment IDs in a directory.
pub fn discover_segment_ids<IO: IoEngine>(dir: &Path, io: &IO) -> Result<Vec<u64>, WalError> {
    let mut ids = Vec::new();
    if !io.exists(dir) {
        return Ok(ids);
    }
    for entry in io.read_dir(dir)? {
        let name = &entry.file_name;
        // Parse "wal-00000001.log" → 1
        if let Some(rest) = name.strip_prefix("wal-")
            && let Some(num_str) = rest.strip_suffix(".log")
                && let Ok(id) = num_str.parse::<u64>() {
                    ids.push(id);
                }
    }
    Ok(ids)
}
