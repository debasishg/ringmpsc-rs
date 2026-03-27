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

## Invariants

### INV-STORE-01: Apply Idempotency Contract

The `WalStore::apply()` implementation is responsible for handling idempotency at the application layer. `ringwal-store` guarantees that `apply()` is called **at most once** per entry within a single `recover_into_store()` invocation. If storage backends require stronger idempotency guarantees across multiple recovery runs (e.g., crash-restart-recover), they must implement deduplication internally (e.g., using the entry's sequence number as a deduplication key).

**Enforcement**: Structural — `apply_transactions()` iterates over `RecoveredTransaction::entries` exactly once.

### INV-STORE-02: Committed Entries Only

`apply()` is **never called** for entries from aborted transactions or for `Commit`/`Abort` marker entries themselves. Only data entries from transactions with `RecoveryAction::Commit` are passed to `apply()`.

```
∀ call to apply(entry): entry ∈ {entries of a committed transaction}
Commit markers, Abort markers, and aborted-transaction entries → never passed to apply()
```

**Enforcement**: Structural — `apply_transactions()` filters by `RecoveryAction::Commit` before iteration.

### INV-STORE-03: Transaction Atomicity

Either all entries from a committed transaction are applied or none are. There is no partial application of a transaction.

```
∀ transaction T with RecoveryAction::Commit:
  (∀ entry ∈ T.entries: apply(entry) succeeds) ∨ (recovery returns Err and no entries are applied)
```

**Enforcement**: `apply_transactions()` returns `Err` on the first `apply()` failure, which propagates through `recover_into_store()`. Callers receive an error rather than partially-applied state.

### INV-STORE-04: InMemoryStore Thread Safety

`InMemoryStore` uses `Arc<RwLock<HashMap<K, V>>>` for thread safety. This is an intentional design choice for the **reference implementation**: it prioritizes simplicity and correctness over throughput. Production implementations targeting high-throughput scenarios should use lockless or sharded alternatives and are not required to use `RwLock`.

## Dependencies

```
ringwal-store
├── ringwal     (WAL engine — recover, WalEntry, WalError, IoEngine, etc.)
└── serde       (DeserializeOwned bounds for recovery generics)
```
