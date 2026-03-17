//! Deterministic simulation testing for `ringwal`.
//!
//! This crate provides an in-memory I/O backend (`SimIo`) with a three-tier
//! write model, fault injection, crash simulation, and a verification oracle.
//! Combined with a seeded RNG and single-threaded tokio runtime, it enables
//! fully reproducible property tests for WAL correctness.
//!
//! # Architecture
//!
//! ```text
//! RingwalSimulator
//!   ├── SimIo: IoEngine      (in-memory FS, 3-tier write model)
//!   │     ├── FaultConfig     (write/fsync/crash failure probabilities)
//!   │     └── SimClock        (deterministic monotonic clock)
//!   └── CommitOracle          (ground truth of acknowledged commits)
//! ```
//!
//! # Quick Start
//!
//! ```ignore
//! use ringwal_sim::{FaultConfig, RingwalSimulator};
//! use ringwal::WalConfig;
//!
//! let fc = FaultConfig::builder()
//!     .write_fail_rate(0.01)
//!     .fsync_fail_rate(0.05)
//!     .build();
//!
//! let mut sim = RingwalSimulator::new(42, fc, WalConfig::new("/wal"));
//! // ... drive workload ...
//! sim.crash_recover_verify().expect("recovery failed");
//! ```

pub mod clock;
pub mod fault;
pub mod harness;
pub mod oracle;
pub mod sim_io;

pub use clock::SimClock;
pub use fault::{FaultConfig, FaultConfigBuilder};
pub use harness::{RingwalSimulator, WorkloadStats};
pub use oracle::{CommitOracle, VerificationResult, verify_recovery};
pub use sim_io::SimIo;
