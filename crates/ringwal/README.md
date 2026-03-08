# ringwal

A Write-Ahead Log backed by lock-free SPSC ring buffers via [`ringmpsc-rs`](../ringmpsc/README.md).

Instead of using a single shared queue (like [`async-wal-db`](https://github.com/debasishg/async-wal-db)),
each writer gets a **dedicated SPSC ring buffer** — zero producer-producer contention.
A single background flusher drains all rings, writes to rotatable segment files,
fsyncs, and notifies commit waiters via **group commit**.

## Quick Start

```rust
use ringwal::{Wal, WalConfig, Transaction};

#[tokio::main]
async fn main() -> Result<(), ringwal::WalError> {
    let config = WalConfig::new("/tmp/my_wal");
    let (mut wal, factory) = Wal::open::<String, Vec<u8>>(config)?;

    let writer = factory.register()?;

    let mut tx = Transaction::new();
    tx.insert("key1".into(), b"value1".to_vec());
    tx.insert("key2".into(), b"value2".to_vec());
    tx.commit(&writer).await?;   // awaits fsync — durable on return

    wal.shutdown().await?;
    Ok(())
}
```

## Key Features

| Feature | Description |
|---------|-------------|
| **Per-writer SPSC rings** | Each `WalWriter` owns a dedicated ring buffer — zero contention between writers |
| **Group commit** | One `fsync` unblocks all commits in the batch via oneshot channels |
| **Segment rotation** | Configurable max segment size; sealed segments can be truncated after checkpoint |
| **CRC32 checksums** | Every entry carries a 13-byte header (length + CRC32 + version) |
| **Generic K/V types** | `WalEntry<K, V>` — any `Serialize + DeserializeOwned` types |
| **Crash recovery** | Multi-segment scan with CRC32 validation and transaction classification |
| **Backpressure** | Built into ring buffers — writers async-wait when full |
| **Configurable** | Ring capacity, max writers, segment size, flush interval, batch hint |

## Configuration

```rust
let config = WalConfig::new("/tmp/wal_dir")
    .with_ring_bits(14)               // 2^14 = 16,384 slots per writer
    .with_max_writers(16)             // up to 16 concurrent writers
    .with_max_segment_size(64 << 20)  // 64 MB per segment file
    .with_flush_interval(Duration::from_millis(10))
    .with_batch_hint(256)
    .with_metrics(false);
```

## Multi-Writer Example

```rust
let (mut wal, factory) = Wal::open::<String, Vec<u8>>(config)?;
let factory = Arc::new(factory);

let mut handles = vec![];
for i in 0..4 {
    let f = Arc::clone(&factory);
    handles.push(tokio::spawn(async move {
        let writer = f.register().unwrap();
        for j in 0..1000 {
            let mut tx = Transaction::new();
            tx.insert(format!("w{i}-k{j}"), vec![i as u8; 64]);
            tx.commit(&writer).await.unwrap();
        }
    }));
}
for h in handles { h.await.unwrap(); }
wal.shutdown().await?;
```

## Recovery

```rust
use ringwal::{recover, read_checkpoint, write_checkpoint, RecoveryAction};

let (transactions, stats) = recover::<String, Vec<u8>>(Path::new("/tmp/wal_dir"))?;
println!("committed: {}, aborted: {}, incomplete: {}",
    stats.committed, stats.aborted, stats.incomplete);

for tx in &transactions {
    if tx.action == RecoveryAction::Commit {
        // replay tx.entries into your storage engine
    }
}

// Checkpoint to allow segment truncation
write_checkpoint(Path::new("/tmp/wal_dir"), wal.current_lsn())?;
```

## On-Disk Format

Each entry on disk:

```
┌────────────┬──────────────┬─────────┬──────────────────────┐
│ length: u64│ checksum: u32│ ver: u8 │ bincode(WalEntry<K,V>)│
│  (8 bytes) │  (4 bytes)   │(1 byte) │    (length bytes)     │
└────────────┴──────────────┴─────────┴──────────────────────┘
```

Segment files: `wal-00000001.log`, `wal-00000002.log`, ...
Checkpoint file: `checkpoint` (contains LSN as little-endian u64).

## Compliance with async-wal-db

### Completed

- [x] Core WAL engine with background flusher
- [x] `WalEntry` enum — Insert / Update / Delete / Commit / Abort
- [x] CRC32 checksums with 13-byte entry header (identical format)
- [x] Transaction abstraction — local buffering, atomic commit/abort
- [x] Crash recovery — segment scan, CRC32 validation, tx classification
- [x] Recovery statistics (`committed`, `aborted`, `incomplete`, `partial_writes`, `checksum_failures`)
- [x] Graceful shutdown with drain of in-flight entries
- [x] Checkpointing — `write_checkpoint()` / `read_checkpoint()`
- [x] Backpressure — async wait when ring buffer is full
- [x] Transaction state tracking (`TxState`: Active / Committed / Aborted)
- [x] Global monotonic tx ID generator with recovery-safe reset
- [x] Error types covering IO, serialization, checksums, partial writes
- [x] Integration tests (9 tests) covering all major paths

### Beyond async-wal-db

- [x] Per-writer SPSC ring buffers (vs shared `SegQueue`)
- [x] Group commit — single fsync unblocks all commits in batch
- [x] Segment rotation with configurable max size
- [x] Sealed segment truncation via `truncate_before(lsn)`
- [x] Generic `K/V` types (async-wal-db is fixed `String/Vec<u8>`)
- [x] Configurable max writers with enforcement
- [x] Configurable batch hint for flusher aggregation
- [x] Optional per-ring metrics via ringmpsc-rs
- [x] LSN-stamped entries (monotonic log sequence numbers)

### Still TODO (for full parity with async-wal-db)

- [x] **`WalStore` trait + `InMemoryStore`** — `WalStore<K,V>` trait with `apply(&mut self, entry)` method, plus `InMemoryStore<K,V>` backed by `Arc<RwLock<HashMap<K,V>>>`. Shipped in `src/store.rs`.
- [x] **Apply-to-store on recovery** — `recover_into_store(dir, store)` and `apply_transactions(store, txns)` bridge WAL recovery to any `WalStore` implementation.
- [ ] **LMDB storage backend** — async-wal-db has `LmdbStorage` (via `heed` crate) with zero-copy reads, batch writes, and 1 GB default map size. ringwal has no persistent storage engine.
- [x] **Automatic checkpoint scheduler** — `Wal::start_checkpoint_scheduler::<K, V>(interval)` runs a background task that periodically calls `checkpoint()` + `truncate_segments_before()`.
- [x] **Checkpoint advancement logic** — `recovery::checkpoint::<K, V>(dir)` scans segments, finds the highest committed `tx_id`, writes a new checkpoint, and returns `WalError::NoNewCheckpoints` if nothing new. `Wal::checkpoint::<K, V>()` also truncates old segments.
- [x] **CLI demo tool** — `bin/demo.rs` (minimal CLI, `cargo run -p ringwal --bin ringwal-demo`) and `examples/demo.rs` (full lifecycle with 4 writers, recovery, checkpointing).
- [x] **Benchmarks** — `benches/wal_throughput.rs` compares ringwal vs async-wal-db via criterion at 1/2/4/8 writer counts. ringwal scales linearly with writers; async-wal-db stays flat. Run with `cargo bench -p ringwal`.
- [x] **README/examples parity** — documentation, examples, and compliance tracking.

## Architecture

See [docs/architecture.md](docs/architecture.md) for the full design document.

## Running Tests

```bash
cargo test -p ringwal --release
```

## License

MIT — see [LICENSE](../../LICENSE).
