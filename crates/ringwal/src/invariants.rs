//! Debug assertion macros for WAL invariants.
//!
//! All macros are no-ops in release builds (`#[cfg(debug_assertions)]`).
//! Each references an `INV-WAL-*` invariant from spec.md.

/// INV-WAL-01: LSN must be strictly monotonically increasing within a segment.
#[allow(unused_macros)]
macro_rules! debug_assert_lsn_monotonic {
    ($prev:expr, $current:expr) => {
        #[cfg(debug_assertions)]
        debug_assert!(
            $current > $prev,
            "INV-WAL-01 violated: LSN {} is not greater than previous {}",
            $current,
            $prev
        );
    };
}

/// INV-WAL-02: Segment file size must not exceed configured maximum.
macro_rules! debug_assert_segment_size {
    ($size:expr, $max:expr) => {
        #[cfg(debug_assertions)]
        debug_assert!(
            $size <= $max,
            "INV-WAL-02 violated: segment size {} exceeds max {}",
            $size,
            $max
        );
    };
}

/// INV-WAL-03: Every persisted entry must have a valid CRC32 checksum.
#[allow(unused_macros)]
macro_rules! debug_assert_entry_checksum {
    ($expected:expr, $actual:expr) => {
        #[cfg(debug_assertions)]
        debug_assert_eq!(
            $expected, $actual,
            "INV-WAL-03 violated: checksum mismatch (expected {:#x}, got {:#x})",
            $expected, $actual
        );
    };
}

/// INV-WAL-04: Segment IDs must be strictly monotonically increasing.
macro_rules! debug_assert_segment_id_monotonic {
    ($prev:expr, $current:expr) => {
        #[cfg(debug_assertions)]
        debug_assert!(
            $current > $prev,
            "INV-WAL-04 violated: segment ID {} is not greater than previous {}",
            $current,
            $prev
        );
    };
}

/// INV-WAL-05: A committed transaction's entries must all be fsynced before
/// the commit waiter is notified.
macro_rules! debug_assert_commit_durable {
    ($fsynced:expr) => {
        #[cfg(debug_assertions)]
        debug_assert!(
            $fsynced,
            "INV-WAL-05 violated: commit notification sent before fsync"
        );
    };
}

#[allow(unused_imports)]
pub(crate) use debug_assert_commit_durable;
#[allow(unused_imports)]
pub(crate) use debug_assert_entry_checksum;
#[allow(unused_imports)]
pub(crate) use debug_assert_lsn_monotonic;
pub(crate) use debug_assert_segment_id_monotonic;
pub(crate) use debug_assert_segment_size;
