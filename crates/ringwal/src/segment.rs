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
pub struct Segment {
    pub(crate) id: u64,
    pub(crate) path: PathBuf,
    pub(crate) file: std::io::BufWriter<std::fs::File>,
    pub(crate) size: u64,
    pub(crate) max_size: u64,
    pub(crate) entry_count: u64,
    pub(crate) first_lsn: u64,
    pub(crate) last_lsn: u64,
}

impl Segment {
    /// Opens or creates a segment file.
    pub fn open(id: u64, dir: &Path, max_size: u64) -> Result<Self, WalError> {
        let path = segment_path(dir, id);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let size = file.metadata()?.len();

        Ok(Self {
            id,
            path,
            file: std::io::BufWriter::new(file),
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

    /// Flushes the buffer and fsyncs data to disk.
    pub fn fsync(&mut self) -> Result<(), WalError> {
        self.file.flush()?;
        self.file.get_ref().sync_all()?;
        Ok(())
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
pub struct SegmentManager {
    dir: PathBuf,
    max_segment_size: u64,
    active: Segment,
    sealed: Vec<SegmentMeta>,
    next_segment_id: u64,
}

impl SegmentManager {
    /// Opens or creates a WAL directory and initialises the first segment.
    pub fn open(dir: &Path, max_segment_size: u64) -> Result<Self, WalError> {
        std::fs::create_dir_all(dir)?;

        // Find existing segments
        let mut segment_ids = discover_segment_ids(dir)?;
        segment_ids.sort();

        let next_id = segment_ids.last().map_or(1, |&id| id + 1);
        let active = Segment::open(next_id, dir, max_segment_size)?;

        Ok(Self {
            dir: dir.to_path_buf(),
            max_segment_size,
            active,
            sealed: Vec::new(),
            next_segment_id: next_id + 1,
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

    /// Flushes and fsyncs the active segment.
    pub fn fsync(&mut self) -> Result<(), WalError> {
        self.active.fsync()
    }

    /// Rotates: seals the current segment and opens a new one.
    pub fn rotate(&mut self) -> Result<(), WalError> {
        let new_id = self.next_segment_id;
        debug_assert_segment_id_monotonic!(self.active.id, new_id);
        self.next_segment_id += 1;

        let new_segment = Segment::open(new_id, &self.dir, self.max_segment_size)?;
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
        self.sealed.retain(|meta| {
            if meta.last_lsn < checkpoint_lsn {
                // Best-effort delete; ignore errors for already-removed files
                let _ = std::fs::remove_file(&meta.path);
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
    dir.join(format!("wal-{:08}.log", id))
}

/// Discovers existing segment IDs in a directory.
pub fn discover_segment_ids(dir: &Path) -> Result<Vec<u64>, WalError> {
    let mut ids = Vec::new();
    if !dir.exists() {
        return Ok(ids);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Parse "wal-00000001.log" → 1
        if let Some(rest) = name.strip_prefix("wal-") {
            if let Some(num_str) = rest.strip_suffix(".log") {
                if let Ok(id) = num_str.parse::<u64>() {
                    ids.push(id);
                }
            }
        }
    }
    Ok(ids)
}
