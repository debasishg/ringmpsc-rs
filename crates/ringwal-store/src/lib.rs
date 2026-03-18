//! Storage backend trait and recovery-to-store bridge for `ringwal`.
//!
//! This crate provides the application-layer abstraction for applying
//! WAL-recovered transactions to a storage engine. The core `ringwal` crate
//! handles writing, flushing, persisting, and recovering WAL entries —
//! its output contract is `Vec<RecoveredTransaction>`. This crate bridges
//! that output to application state via the [`WalStore`] trait.
//!
//! # Provided types
//!
//! - [`WalStore<K, V>`] — trait for storage backends (implement for LMDB, RocksDB, etc.)
//! - [`InMemoryStore<K, V>`] — thread-safe `HashMap`-backed reference implementation
//! - [`recover_into_store()`] — main entry point: recover WAL segments and replay into a store
//! - [`apply_transactions()`] — replay a set of recovered transactions into a store
//!
//! # Example
//!
//! ```ignore
//! use ringwal::RealIo;
//! use ringwal_store::{InMemoryStore, recover_into_store};
//!
//! let mut store = InMemoryStore::<String, Vec<u8>>::new();
//! let stats = recover_into_store::<String, Vec<u8>, _, _>(dir, &mut store, &RealIo)?;
//! let snapshot = store.snapshot();
//! ```

mod store;

pub use store::{apply_transactions, recover_into_store, InMemoryStore, WalStore};
