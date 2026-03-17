//! WAL entry types and on-disk header format.

use serde::{Deserialize, Serialize};

use crate::error::WalError;

/// A write-ahead log entry.
///
/// Generic over key type `K` and value type `V` to support different
/// storage backends. Both must be serializable for on-disk persistence.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WalEntry<K, V> {
    /// Insert a new key-value pair.
    Insert {
        tx_id: u64,
        timestamp: u64,
        key: K,
        value: V,
    },
    /// Update an existing key's value.
    Update {
        tx_id: u64,
        timestamp: u64,
        key: K,
        old_value: V,
        new_value: V,
    },
    /// Delete a key.
    Delete {
        tx_id: u64,
        timestamp: u64,
        key: K,
        old_value: V,
    },
    /// Mark a transaction as committed (durable after fsync).
    Commit { tx_id: u64, timestamp: u64 },
    /// Mark a transaction as aborted.
    Abort { tx_id: u64, timestamp: u64 },
}

impl<K, V> WalEntry<K, V> {
    /// Returns the transaction ID of this entry.
    pub fn tx_id(&self) -> u64 {
        match self {
            Self::Insert { tx_id, .. }
            | Self::Update { tx_id, .. }
            | Self::Delete { tx_id, .. }
            | Self::Commit { tx_id, .. }
            | Self::Abort { tx_id, .. } => *tx_id,
        }
    }

    /// Returns the timestamp of this entry.
    pub fn timestamp(&self) -> u64 {
        match self {
            Self::Insert { timestamp, .. }
            | Self::Update { timestamp, .. }
            | Self::Delete { timestamp, .. }
            | Self::Commit { timestamp, .. }
            | Self::Abort { timestamp, .. } => *timestamp,
        }
    }

    /// Returns `true` if this is a commit marker.
    pub fn is_commit(&self) -> bool {
        matches!(self, Self::Commit { .. })
    }

    /// Returns `true` if this is an abort marker.
    pub fn is_abort(&self) -> bool {
        matches!(self, Self::Abort { .. })
    }

    /// Creates a new timestamp from the system clock.
    #[must_use] 
    pub fn new_timestamp() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }
}

/// Convenience type alias for `WalEntry<String, Vec<u8>>`.
pub type ByteWalEntry = WalEntry<String, Vec<u8>>;

/// On-disk header prepended to each serialized WAL entry.
///
/// Format (13 bytes):
/// ```text
/// [length: u64 LE][checksum: u32 LE][version: u8]
/// ```
#[derive(Debug, Clone, Copy)]
pub struct WalEntryHeader {
    /// Length of the serialized entry data (not including header).
    pub length: u64,
    /// CRC32 checksum of the serialized entry data.
    pub checksum: u32,
    /// Format version (currently always 1).
    pub version: u8,
}

impl WalEntryHeader {
    /// Current WAL format version.
    pub const VERSION: u8 = 1;

    /// Size of header in bytes (8 + 4 + 1 = 13).
    pub const SIZE: usize = 13;

    /// Creates a new header for the given serialized entry data.
    #[must_use] 
    pub fn new(data: &[u8]) -> Self {
        let checksum = crc32fast::hash(data);
        Self {
            length: data.len() as u64,
            checksum,
            version: Self::VERSION,
        }
    }

    /// Serializes the header to bytes.
    #[must_use] 
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut bytes = [0u8; Self::SIZE];
        bytes[0..8].copy_from_slice(&self.length.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.checksum.to_le_bytes());
        bytes[12] = self.version;
        bytes
    }

    /// Deserializes a header from bytes.
    #[must_use] 
    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> Self {
        let length = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let checksum = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let version = bytes[12];
        Self {
            length,
            checksum,
            version,
        }
    }

    /// Validates that the data matches this header's checksum.
    pub fn validate(&self, data: &[u8]) -> Result<(), WalError> {
        let actual = crc32fast::hash(data);
        if actual != self.checksum {
            return Err(WalError::ChecksumMismatch {
                expected: self.checksum,
                actual,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let data = b"hello world";
        let header = WalEntryHeader::new(data);
        let bytes = header.to_bytes();
        let restored = WalEntryHeader::from_bytes(&bytes);
        assert_eq!(header.length, restored.length);
        assert_eq!(header.checksum, restored.checksum);
        assert_eq!(header.version, restored.version);
        restored.validate(data).unwrap();
    }

    #[test]
    fn header_detects_corruption() {
        let data = b"hello world";
        let header = WalEntryHeader::new(data);
        let corrupt = b"hello worle";
        assert!(header.validate(corrupt).is_err());
    }

    #[test]
    fn entry_roundtrip() {
        let entry: ByteWalEntry = WalEntry::Insert {
            tx_id: 1,
            timestamp: 123_456,
            key: "test_key".to_string(),
            value: b"test_value".to_vec(),
        };
        let serialized = bincode::serialize(&entry).unwrap();
        let deserialized: ByteWalEntry = bincode::deserialize(&serialized).unwrap();
        assert_eq!(entry, deserialized);
    }

    #[test]
    fn entry_accessors() {
        let entry: ByteWalEntry = WalEntry::Commit {
            tx_id: 42,
            timestamp: 100,
        };
        assert_eq!(entry.tx_id(), 42);
        assert_eq!(entry.timestamp(), 100);
        assert!(entry.is_commit());
        assert!(!entry.is_abort());
    }
}
