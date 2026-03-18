# ringwal-store — Storage Backend Specification

## Overview

`ringwal-store` provides the application-layer abstraction for applying
WAL-recovered transactions to a storage engine. The core `ringwal` crate
handles writing, flushing, persisting, and recovering WAL entries — its
output contract is `Vec<RecoveredTransaction<K, V>>` + `RecoveryStats`.
This crate bridges that output to application state.

## Scope Boundary

**ringwal** is responsible for:
- Writing entries to SPSC rings, flushing to segments, fsync
- Crash recovery: scanning segments, CRC32 validation, transaction classification
- Checkpointing and segment truncation
- Output: `Vec<RecoveredTransaction<K, V>>` + `RecoveryStats`

**ringwal-store** is responsible for:
- Defining how recovered entries are applied to a storage backend (`WalStore` trait)
- Providing a reference in-memory implementation (`InMemoryStore`)
- Orchestrating recovery-to-store replay (`recover_into_store`, `apply_transactions`)

## Public API

### `WalStore<K, V>` trait

Defines a single method:

```rust
fn apply(&mut self, entry: &WalEntry<K, V>) -> Result<(), WalError>;
```

- Only called for entries from **committed** transactions
- `Commit` and `Abort` marker entries are never passed to `apply()`
- Implement for `HashMap`, LMDB, RocksDB, or any key-value store

### `InMemoryStore<K, V>`

Thread-safe `Arc<RwLock<HashMap<K, V>>>` reference implementation.

- `new()` — create empty store
- `snapshot()` — clone current state
- `len()`, `is_empty()`, `get()` — read access
- Implements `WalStore<K, V>` for `K: Eq + Hash + Clone, V: Clone`

### `recover_into_store(dir, store, io) -> Result<RecoveryStats>`

Main entry point for crash recovery with a storage backend:
1. Calls `ringwal::recover()` to scan segments
2. Calls `apply_transactions()` to replay committed entries
3. Returns `RecoveryStats`

### `apply_transactions(store, transactions) -> Result<()>`

Filters `RecoveryAction::Commit` transactions and replays entries via `store.apply()`.

## Dependencies

```
ringwal-store
├── ringwal     (WAL engine — recover, WalEntry, WalError, IoEngine, etc.)
└── serde       (DeserializeOwned bounds for recovery generics)
```
