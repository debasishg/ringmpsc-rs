# ringwal — Ring-Buffer Write-Ahead Log Specification

## Overview

`ringwal` is a Write-Ahead Log backed by multiple SPSC ring buffers from `ringmpsc-rs`.
Each writer gets a dedicated lock-free ring, enabling parallel writes with zero
producer-producer contention. A single background flusher drains all rings,
serializes entries, writes to rotatable segment files, and provides group commit
durability via fsync.

## Invariants

### INV-WAL-01: LSN Monotonicity
LSN (Log Sequence Number) values assigned by the flusher are strictly monotonically
increasing within and across segments. The flusher is the sole LSN allocator.

### INV-WAL-02: Segment Size Bound
A segment file's size must not exceed `max_segment_size` by more than one entry.
Rotation is triggered before writing when the current size >= max.

### INV-WAL-03: Entry Integrity
Every persisted entry has a valid CRC32 checksum in its header. Corruption is
detected during recovery by recomputing the checksum.

### INV-WAL-04: Segment ID Monotonicity
Segment IDs are strictly monotonically increasing. New segments always get
`max(existing) + 1`.

### INV-WAL-05: Commit Durability
A commit waiter is only notified after the batch containing its commit marker
has been written to the active segment and fsynced. This is the group commit
guarantee.

### INV-WAL-06: Per-Writer SPSC Invariant
Each `WalWriter` is backed by exactly one SPSC ring buffer. The writer is the
sole producer; the flusher is the sole consumer. No locks on the write path.

### INV-WAL-07: Transaction Atomicity
A transaction's entries are either all recoverable (committed) or all discarded
(aborted / incomplete). The `Commit` marker is the linearization point.

## On-Disk Format

### Entry Format
```
[WalEntryHeader: 13 bytes][Serialized WalEntry: N bytes]
```

### Header (13 bytes)
| Offset | Size | Field    | Description                |
|--------|------|----------|----------------------------|
| 0      | 8    | length   | u64 LE — data length       |
| 8      | 4    | checksum | u32 LE — CRC32 of data     |
| 12     | 1    | version  | u8 — format version (= 1)  |

### Segment Files
Named `wal-{id:08}.log` (e.g., `wal-00000001.log`).
Checkpoint stored in `checkpoint` file as 8-byte LE u64 (LSN).

## Recovery Protocol

1. Discover all `wal-*.log` files, sort by ID ascending.
2. Read each file sequentially, validating header + CRC32 per entry.
3. Stop at first corruption (partial write / checksum mismatch).
4. Group entries by `tx_id`, classify as Commit / Abort / Incomplete.
5. Return only committed transactions' entries.
6. Reset `NEXT_TX_ID` to `max(recovered_tx_ids) + 1`.

## Ordering Guarantees

- **Per-writer FIFO**: Entries from a single writer are ordered.
- **Cross-writer**: Interleaved by flusher drain order (round-robin across rings).
  No strict global total order across writers. Per-key ordering is preserved
  if each key is written by a single writer.
