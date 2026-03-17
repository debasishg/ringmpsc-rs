//! WAL recovery and checkpoint support.
//!
//! Recovery reads all segment files in order, validates each entry's CRC32
//! checksum, classifies transactions (committed / aborted / incomplete),
//! and reports statistics. Partial writes at segment EOF are truncated.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use serde::de::DeserializeOwned;

use crate::entry::{WalEntry, WalEntryHeader};
use crate::error::WalError;
use crate::io::{IoEngine, ReadHandle};
use crate::segment::{discover_segment_ids, segment_path};
use crate::writer::reset_tx_id;

/// Classification of a transaction's recovery outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Transaction had a Commit marker.
    Commit,
    /// Transaction had an Abort marker.
    Rollback,
    /// Transaction had neither — treated as rolled back.
    Incomplete,
}

/// Statistics from a recovery scan.
#[derive(Debug, Default, Clone)]
pub struct RecoveryStats {
    pub total_transactions: usize,
    pub committed: usize,
    pub aborted: usize,
    pub incomplete: usize,
    pub partial_writes: usize,
    pub checksum_failures: usize,
}

/// Result of recovering a single transaction.
#[derive(Debug)]
pub struct RecoveredTransaction<K, V> {
    pub tx_id: u64,
    pub action: RecoveryAction,
    pub entries: Vec<WalEntry<K, V>>,
}

/// Recovers WAL state from all segment files in `dir`.
///
/// Returns the set of recovered transactions and aggregate statistics.
pub fn recover<K, V, IO>(
    dir: &Path,
    io: &IO,
) -> Result<(Vec<RecoveredTransaction<K, V>>, RecoveryStats), WalError>
where
    K: DeserializeOwned + Send + 'static,
    V: DeserializeOwned + Send + 'static,
    IO: IoEngine,
{
    let mut segment_ids = discover_segment_ids(dir, io)?;
    segment_ids.sort_unstable();

    let mut all_entries: Vec<WalEntry<K, V>> = Vec::new();
    let mut stats = RecoveryStats::default();

    for &seg_id in &segment_ids {
        let path = segment_path(dir, seg_id);
        read_segment_entries(&path, &mut all_entries, &mut stats, io)?;
    }

    // Classify transactions
    let mut tx_entries: HashMap<u64, Vec<WalEntry<K, V>>> = HashMap::new();
    let mut tx_status: HashMap<u64, RecoveryAction> = HashMap::new();
    let mut max_tx_id: u64 = 0;

    for entry in all_entries {
        let tx_id = entry.tx_id();
        if tx_id > max_tx_id {
            max_tx_id = tx_id;
        }

        if entry.is_commit() {
            tx_status.insert(tx_id, RecoveryAction::Commit);
        } else if entry.is_abort() {
            tx_status.insert(tx_id, RecoveryAction::Rollback);
        } else {
            tx_entries.entry(tx_id).or_default().push(entry);
        }
    }

    // Build recovered transactions
    let mut recovered = Vec::new();
    let all_tx_ids: Vec<u64> = tx_entries
        .keys()
        .copied()
        .chain(tx_status.keys().copied())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    for tx_id in all_tx_ids {
        let action = tx_status
            .get(&tx_id)
            .copied()
            .unwrap_or(RecoveryAction::Incomplete);
        let entries = tx_entries.remove(&tx_id).unwrap_or_default();

        match action {
            RecoveryAction::Commit => stats.committed += 1,
            RecoveryAction::Rollback => stats.aborted += 1,
            RecoveryAction::Incomplete => stats.incomplete += 1,
        }

        recovered.push(RecoveredTransaction {
            tx_id,
            action,
            entries,
        });
    }

    stats.total_transactions = recovered.len();

    // Reset TX ID counter to avoid reuse
    if max_tx_id > 0 {
        reset_tx_id(max_tx_id + 1);
    }

    Ok((recovered, stats))
}

/// Reads all valid entries from a single segment file.
fn read_segment_entries<K, V, IO>(
    path: &Path,
    entries: &mut Vec<WalEntry<K, V>>,
    stats: &mut RecoveryStats,
    io: &IO,
) -> Result<(), WalError>
where
    K: DeserializeOwned,
    V: DeserializeOwned,
    IO: IoEngine,
{
    let mut file = match io.open_read(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    let file_len = file.metadata_len()?;
    let mut offset: u64 = 0;

    loop {
        if offset + WalEntryHeader::SIZE as u64 > file_len {
            if offset < file_len {
                // Trailing bytes that don't form a complete header
                stats.partial_writes += 1;
            }
            break;
        }

        // Read header
        let mut header_bytes = [0u8; WalEntryHeader::SIZE];
        if file.read_exact(&mut header_bytes).is_err() {
            stats.partial_writes += 1;
            break;
        }
        let header = WalEntryHeader::from_bytes(&header_bytes);
        offset += WalEntryHeader::SIZE as u64;

        // Sanity check: data length must not exceed remaining file
        if offset + header.length > file_len {
            stats.partial_writes += 1;
            break;
        }

        // Read entry data
        let mut data = vec![0u8; header.length as usize];
        if file.read_exact(&mut data).is_err() {
            stats.partial_writes += 1;
            break;
        }
        offset += header.length;

        // Validate checksum
        if header.validate(&data).is_err() {
            stats.checksum_failures += 1;
            // Stop at first corruption — tail may be torn
            break;
        }

        // Deserialize
        if let Ok(entry) = bincode::deserialize::<WalEntry<K, V>>(&data) { entries.push(entry) } else {
            stats.checksum_failures += 1;
            break;
        }
    }

    Ok(())
}

/// Writes a checkpoint file recording the highest durable LSN.
pub fn write_checkpoint<IO: IoEngine>(dir: &Path, lsn: u64, io: &IO) -> Result<(), WalError> {
    let path = dir.join("checkpoint");
    let data = lsn.to_le_bytes();
    io.write_file_bytes(&path, &data)?;
    Ok(())
}

/// Reads the last checkpoint LSN, or 0 if no checkpoint exists.
pub fn read_checkpoint<IO: IoEngine>(dir: &Path, io: &IO) -> Result<u64, WalError> {
    let path = dir.join("checkpoint");
    match io.read_file_bytes(&path) {
        Ok(data) if data.len() == 8 => {
            let bytes: [u8; 8] = data.try_into().unwrap();
            Ok(u64::from_le_bytes(bytes))
        }
        Ok(_) => Ok(0),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e.into()),
    }
}

/// Advances the checkpoint to the highest committed transaction's LSN.
///
/// Scans all segment files, identifies committed transactions, and writes
/// a checkpoint at the highest committed LSN. Returns `NoNewCheckpoints`
/// if no committed transactions exist beyond the current checkpoint.
///
/// This is the analog of async-wal-db's `checkpoint()` method which
/// filters committed-only transactions and advances to the highest
/// committed `tx_id`.
pub fn checkpoint<K, V, IO>(dir: &Path, io: &IO) -> Result<u64, WalError>
where
    K: DeserializeOwned + Send + 'static,
    V: DeserializeOwned + Send + 'static,
    IO: IoEngine,
{
    let current_lsn = read_checkpoint(dir, io)?;
    let (recovered, _stats) = recover::<K, V, IO>(dir, io)?;

    // Find the highest tx_id among committed transactions
    let max_committed_tx_id = recovered
        .iter()
        .filter(|t| t.action == RecoveryAction::Commit)
        .map(|t| t.tx_id)
        .max();

    match max_committed_tx_id {
        Some(tx_id) if tx_id > current_lsn => {
            write_checkpoint(dir, tx_id, io)?;
            Ok(tx_id)
        }
        Some(_) | None => Err(WalError::NoNewCheckpoints),
    }
}

/// Removes segment files whose names indicate an ID strictly less
/// than the segment containing the given checkpoint LSN.
///
/// This is a directory-level operation that does not require access to
/// the live `SegmentManager`. It complements `SegmentManager::truncate_before()`
/// for use outside the flusher task (e.g., from the checkpoint scheduler).
pub fn truncate_segments_before<IO: IoEngine>(dir: &Path, checkpoint_lsn: u64, io: &IO) -> Result<usize, WalError> {
    let mut segment_ids = discover_segment_ids(dir, io)?;
    segment_ids.sort_unstable();

    // Keep at least the last segment (even if empty) to avoid removing the active one
    if segment_ids.len() <= 1 {
        return Ok(0);
    }

    let mut removed = 0;
    // Never remove the last segment — it might be the active one
    let candidates = &segment_ids[..segment_ids.len() - 1];

    for &seg_id in candidates {
        let path = segment_path(dir, seg_id);
        // Read the segment to find its max tx_id.
        // We read each segment cheaply with the raw header parser
        // to find the highest tx_id. If it's below the checkpoint,
        // every entry in that segment is already checkpointed.
        let max_tx_id = read_segment_max_tx_id(&path, io)?;
        if max_tx_id > 0 && max_tx_id < checkpoint_lsn {
            let _ = io.remove_file(&path);
            removed += 1;
        }
    }

    Ok(removed)
}

/// Reads a segment file and returns the maximum `tx_id` found, or 0 if empty/unreadable.
fn read_segment_max_tx_id<IO: IoEngine>(path: &Path, io: &IO) -> Result<u64, WalError> {
    let mut file = match io.open_read(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };

    let file_len = file.metadata_len()?;
    let mut offset: u64 = 0;
    let mut max_tx_id: u64 = 0;

    loop {
        if offset + WalEntryHeader::SIZE as u64 > file_len {
            break;
        }

        let mut header_bytes = [0u8; WalEntryHeader::SIZE];
        if file.read_exact(&mut header_bytes).is_err() {
            break;
        }
        let header = WalEntryHeader::from_bytes(&header_bytes);
        offset += WalEntryHeader::SIZE as u64;

        if offset + header.length > file_len {
            break;
        }

        let mut data = vec![0u8; header.length as usize];
        if file.read_exact(&mut data).is_err() {
            break;
        }
        offset += header.length;

        if header.validate(&data).is_err() {
            break;
        }

        // Deserialize as ByteWalEntry to extract tx_id
        if let Ok(entry) = bincode::deserialize::<crate::entry::ByteWalEntry>(&data) {
            let tx_id = entry.tx_id();
            if tx_id > max_tx_id {
                max_tx_id = tx_id;
            }
        }
    }

    Ok(max_tx_id)
}
