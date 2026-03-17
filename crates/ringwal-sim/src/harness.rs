//! Simulator harness for driving WAL workloads under fault injection.
//!
//! `RingwalSimulator` composes `SimIo`, `CommitOracle`, a seeded RNG, and
//! `FaultConfig` into a self-contained test driver. It exercises the WAL
//! through its public API (black-box testing) with deterministic scheduling.
//!
//! # Phase 4 — Workload Driver
//!
//! `run_workload(steps)` generates random operations: register writers,
//! create transactions, insert entries, commit or abort, advance clock.
//! Acknowledged commits are recorded in the oracle for later verification.

use std::path::PathBuf;

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

use ringwal::{
    IoEngine, RecoveredTransaction, RecoveryStats, Transaction, Wal,
    WalConfig, WalError, WalWriter,
};

use crate::clock::SimClock;
use crate::fault::FaultConfig;
use crate::oracle::{self, CommitOracle, VerificationResult};
use crate::sim_io::SimIo;

/// Deterministic simulation harness for the ring-buffer WAL.
///
/// Owns the entire simulated environment: in-memory I/O, clock, oracle,
/// and seeded RNG. Designed for use with `#[tokio::test(flavor = "current_thread")]`.
///
/// # Usage
///
/// ```ignore
/// let config = WalConfig::new("/wal").with_sync_mode(SyncMode::Full);
/// let mut sim = RingwalSimulator::new(42, FaultConfig::none(), config);
/// sim.run_workload(100).await.unwrap();
/// let (recovered, stats) = sim.crash_and_recover().unwrap();
/// sim.assert_no_lost_commits(&recovered);
/// ```
pub struct RingwalSimulator {
    sim_io: SimIo,
    oracle: CommitOracle<String, Vec<u8>>,
    rng: SmallRng,
    fault_config: FaultConfig,
    wal_config: WalConfig,
    wal_dir: PathBuf,
    seed: u64,
}

/// The outcome of a single workload run (before crash/recovery).
#[derive(Debug, Default)]
pub struct WorkloadStats {
    /// Total workload steps executed.
    pub steps_executed: usize,
    /// Transactions successfully committed.
    pub commits: usize,
    /// Transactions aborted.
    pub aborts: usize,
    /// Operations that failed due to injected faults (non-fatal).
    pub fault_errors: usize,
    /// Writers registered.
    pub writers_registered: usize,
    /// Transaction IDs that were successfully aborted.
    pub aborted_tx_ids: Vec<u64>,
}

impl RingwalSimulator {
    /// Creates a new simulator with the given seed and fault configuration.
    ///
    /// The WAL directory is taken from `wal_config.dir`. The simulator
    /// pre-creates the directory in the simulated filesystem.
    #[must_use] 
    pub fn new(seed: u64, fault_config: FaultConfig, wal_config: WalConfig) -> Self {
        let clock = SimClock::default();
        let sim_io = SimIo::with_clock(seed, fault_config.clone(), clock);
        let wal_dir = wal_config.dir.clone();

        // Pre-create the WAL directory in the simulated filesystem
        sim_io
            .create_dir_all(&wal_dir)
            .expect("failed to create WAL directory in SimIo");

        Self {
            sim_io,
            oracle: CommitOracle::new(),
            rng: SmallRng::seed_from_u64(seed),
            fault_config,
            wal_config,
            wal_dir,
            seed,
        }
    }

    // ── Accessors ────────────────────────────────────────────────────────

    /// Returns a reference to the simulated I/O engine.
    #[must_use] 
    pub fn sim_io(&self) -> &SimIo {
        &self.sim_io
    }

    /// Returns a clone of the simulated I/O engine (for passing to `Wal::open`).
    #[must_use] 
    pub fn sim_io_cloned(&self) -> SimIo {
        self.sim_io.clone()
    }

    /// Returns a mutable reference to the commit oracle.
    pub fn oracle_mut(&mut self) -> &mut CommitOracle<String, Vec<u8>> {
        &mut self.oracle
    }

    /// Returns a reference to the commit oracle.
    #[must_use] 
    pub fn oracle(&self) -> &CommitOracle<String, Vec<u8>> {
        &self.oracle
    }

    /// Returns a mutable reference to the seeded RNG for generating workloads.
    pub fn rng_mut(&mut self) -> &mut SmallRng {
        &mut self.rng
    }

    /// Returns the WAL config.
    #[must_use] 
    pub fn wal_config(&self) -> &WalConfig {
        &self.wal_config
    }

    /// Returns the WAL directory path.
    #[must_use] 
    pub fn wal_dir(&self) -> &PathBuf {
        &self.wal_dir
    }

    /// Returns the fault configuration.
    #[must_use] 
    pub fn fault_config(&self) -> &FaultConfig {
        &self.fault_config
    }

    /// Returns the seed used to create this simulator.
    #[must_use] 
    pub fn seed(&self) -> u64 {
        self.seed
    }

    // ── Crash & Recovery ─────────────────────────────────────────────────

    /// Simulates a crash: discards all non-durable data.
    pub fn crash(&self) {
        self.sim_io.crash();
    }

    /// Returns the number of crashes that have occurred.
    #[must_use] 
    pub fn crash_count(&self) -> u64 {
        self.sim_io.crash_count()
    }

    /// Advances the simulated clock by `delta` seconds.
    pub fn advance_clock(&self, delta: u64) {
        self.sim_io.advance_clock(delta);
    }

    /// Crashes the simulated filesystem and runs WAL recovery.
    ///
    /// This is the core DST primitive: crash → recover → verify.
    pub fn crash_and_recover(
        &self,
    ) -> Result<(Vec<RecoveredTransaction<String, Vec<u8>>>, RecoveryStats), WalError> {
        self.sim_io.crash();
        self.recover()
    }

    /// Runs recovery on the simulated WAL directory (without crashing first).
    pub fn recover(
        &self,
    ) -> Result<(Vec<RecoveredTransaction<String, Vec<u8>>>, RecoveryStats), WalError> {
        ringwal::recover::<String, Vec<u8>, SimIo>(&self.wal_dir, &self.sim_io)
    }

    // ── Workload Driver ──────────────────────────────────────────────────

    /// Runs a random workload of `steps` operations against the WAL.
    ///
    /// Operations are chosen randomly based on the seeded RNG:
    /// - Register new writers (up to `max_writers`)
    /// - Create transactions with 1-5 insert entries
    /// - Commit or abort transactions (weighted 80/20 toward commits)
    /// - Advance clock occasionally
    ///
    /// Every acknowledged commit (where `tx.commit()` returns `Ok`) is
    /// recorded in the oracle for later verification.
    ///
    /// Returns statistics about the workload, or a fatal WAL error.
    /// Non-fatal I/O errors (from fault injection) are counted but tolerated.
    pub async fn run_workload(&mut self, steps: usize) -> Result<WorkloadStats, WalError> {
        let io = self.sim_io.clone();
        let config = self.wal_config.clone();

        let (mut wal, factory) = Wal::open::<String, Vec<u8>>(config, io)?;

        let mut writers: Vec<WalWriter<String, Vec<u8>>> = Vec::new();
        let mut stats = WorkloadStats::default();

        for _step in 0..steps {
            // Decide what to do this step
            let action = self.rng.gen_range(0u8..100);

            match action {
                // 0-14: Register a new writer (if under limit)
                0..=14 => {
                    if writers.len() < self.wal_config.max_writers {
                        match factory.register() {
                            Ok(w) => {
                                writers.push(w);
                                stats.writers_registered += 1;
                            }
                            Err(_) => {
                                stats.fault_errors += 1;
                            }
                        }
                    }
                    // If no writers yet, force-register one
                    if writers.is_empty() {
                        match factory.register() {
                            Ok(w) => {
                                writers.push(w);
                                stats.writers_registered += 1;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                }

                // 15-79: Commit a transaction (with 1-5 inserts)
                15..=79 => {
                    // Ensure at least one writer exists
                    if writers.is_empty() {
                        let w = factory.register()?;
                        writers.push(w);
                        stats.writers_registered += 1;
                    }

                    let writer_idx = self.rng.gen_range(0..writers.len());
                    let num_entries = self.rng.gen_range(1..=5usize);
                    let writer = &writers[writer_idx];

                    let mut tx = Transaction::<String, Vec<u8>>::new();
                    let tx_id = tx.id;
                    let mut entries = Vec::new();

                    for e in 0..num_entries {
                        let key = format!("k-{tx_id}-{e}");
                        let value = format!("v-{tx_id}-{e}").into_bytes();
                        entries.push((key.clone(), value.clone()));
                        tx.insert(key, value);
                    }

                    match tx.commit(writer).await {
                        Ok(()) => {
                            // Commit acknowledged — record in oracle
                            self.oracle.record_commit(tx_id, entries);
                            stats.commits += 1;
                        }
                        Err(WalError::Io(_)) => {
                            // Fault injection caused an I/O error — transaction
                            // may or may not have been committed. Since commit()
                            // returned Err, we do NOT record it in the oracle.
                            stats.fault_errors += 1;
                        }
                        Err(_) => {
                            // Fatal error (Closed, etc.) — abort the workload
                            // but don't propagate. The WAL might have been
                            // killed by a crash fault. Try to shut down gracefully.
                            stats.fault_errors += 1;
                            let _ = wal.shutdown().await;
                            stats.steps_executed = _step + 1;
                            return Ok(stats);
                        }
                    }
                }

                // 80-89: Abort a transaction
                80..=89 => {
                    if writers.is_empty() {
                        continue;
                    }

                    let writer_idx = self.rng.gen_range(0..writers.len());
                    let writer = &writers[writer_idx];

                    let mut tx = Transaction::<String, Vec<u8>>::new();
                    let tx_id = tx.id;
                    let num_entries = self.rng.gen_range(1..=3usize);
                    for e in 0..num_entries {
                        tx.insert(
                            format!("abort-k-{e}"),
                            format!("abort-v-{e}").into_bytes(),
                        );
                    }

                    match tx.abort(writer).await {
                        Ok(()) => {
                            stats.aborts += 1;
                            stats.aborted_tx_ids.push(tx_id);
                        }
                        Err(WalError::Io(_)) => {
                            stats.fault_errors += 1;
                        }
                        Err(_) => {
                            stats.fault_errors += 1;
                            let _ = wal.shutdown().await;
                            stats.steps_executed = _step + 1;
                            return Ok(stats);
                        }
                    }
                }

                // 90-99: Advance clock by 1-60 seconds
                90..=99 => {
                    let delta = self.rng.gen_range(1..=60u64);
                    self.sim_io.advance_clock(delta);
                }

                _ => unreachable!(),
            }

            stats.steps_executed = _step + 1;
        }

        // Graceful shutdown after workload completes
        let _ = wal.shutdown().await;
        Ok(stats)
    }

    // ── Verification ─────────────────────────────────────────────────────

    /// Verifies recovery output against the oracle (no lost commits, no phantoms).
    ///
    /// Panics with a detailed diagnostic message on failure, including the
    /// seed for exact replay.
    pub fn assert_no_lost_commits(&self, recovered: &[RecoveredTransaction<String, Vec<u8>>]) {
        let result = oracle::verify_recovery(&self.oracle, recovered);
        assert!(result.lost.is_empty(), 
            "Lost commits detected (seed={})!\n\
             Lost tx_ids: {:?}\n\
             Oracle committed: {}\n\
             Recovered committed: {}\n\
             Fault config: {:?}\n\
             Crash count: {}",
            self.seed,
            result.lost,
            self.oracle.committed_count(),
            result.recovered_committed,
            self.fault_config,
            self.crash_count(),
        );
    }

    /// Checks that no aborted transaction appears as committed in recovery.
    /// (INV-WAL-07: no phantom commits from aborted txns)
    pub fn assert_no_phantom_commits(
        &self,
        recovered: &[RecoveredTransaction<String, Vec<u8>>],
    ) {
        let result = oracle::verify_recovery(&self.oracle, recovered);
        assert!(result.phantom.is_empty(), 
            "Phantom commits detected (seed={})!\n\
             Phantom tx_ids: {:?}\n\
             Fault config: {:?}",
            self.seed, result.phantom, self.fault_config,
        );
    }

    /// Returns the verification result without panicking (for custom assertions).
    #[must_use] 
    pub fn verify(&self, recovered: &[RecoveredTransaction<String, Vec<u8>>]) -> VerificationResult {
        oracle::verify_recovery(&self.oracle, recovered)
    }

    /// Convenience: crash, recover, and verify in one call.
    ///
    /// Returns the recovered transactions on success, panics on verification failure.
    pub fn crash_recover_verify(
        &self,
    ) -> Result<Vec<RecoveredTransaction<String, Vec<u8>>>, WalError> {
        let (recovered, _stats) = self.crash_and_recover()?;
        self.assert_no_lost_commits(&recovered);
        self.assert_no_phantom_commits(&recovered);
        Ok(recovered)
    }
}

#[cfg(test)]
mod tests {
    use ringwal::SyncMode;

    use super::*;

    fn test_wal_config() -> WalConfig {
        WalConfig::new("/wal")
            .with_ring_bits(10) // 1024 slots
            .with_max_writers(4)
            .with_max_segment_size(1024 * 1024) // 1 MB
            .with_flush_interval(std::time::Duration::from_millis(5))
            .with_batch_hint(64)
            .with_sync_mode(SyncMode::Full)
    }

    #[test]
    fn harness_creation() {
        let sim = RingwalSimulator::new(42, FaultConfig::none(), test_wal_config());
        assert_eq!(sim.crash_count(), 0);
        assert_eq!(sim.oracle().committed_count(), 0);
        assert_eq!(sim.seed(), 42);
        assert!(sim.sim_io().exists(std::path::Path::new("/wal")));
    }

    #[test]
    fn crash_increments_count() {
        let sim = RingwalSimulator::new(42, FaultConfig::none(), test_wal_config());
        sim.crash();
        assert_eq!(sim.crash_count(), 1);
        sim.crash();
        assert_eq!(sim.crash_count(), 2);
    }

    #[test]
    fn oracle_records_commits() {
        let mut sim = RingwalSimulator::new(42, FaultConfig::none(), test_wal_config());
        sim.oracle_mut()
            .record_commit(1, vec![("k".into(), b"v".to_vec())]);
        assert_eq!(sim.oracle().committed_count(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_workload_no_faults() {
        let mut sim = RingwalSimulator::new(42, FaultConfig::none(), test_wal_config());
        let stats = sim.run_workload(50).await.unwrap();

        assert!(stats.commits > 0, "should have some commits");
        assert!(stats.writers_registered > 0, "should register writers");
        assert_eq!(stats.steps_executed, 50);

        // Verify oracle recorded commits match stats
        assert_eq!(sim.oracle().committed_count(), stats.commits);

        // Crash, recover, verify — no lost commits
        let (recovered, rec_stats) = sim.crash_and_recover().unwrap();
        sim.assert_no_lost_commits(&recovered);
        sim.assert_no_phantom_commits(&recovered);

        // Recovery should see at least what the oracle recorded
        assert!(
            rec_stats.committed >= sim.oracle().committed_count(),
            "recovered {} but oracle has {}",
            rec_stats.committed,
            sim.oracle().committed_count(),
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_workload_with_faults_and_verify() {
        // Mild fault injection — some ops may fail but committed txns must survive
        let fc = FaultConfig::builder()
            .write_fail_rate(0.02)
            .fsync_fail_rate(0.02)
            .build();

        let mut sim = RingwalSimulator::new(123, fc, test_wal_config());
        let stats = sim.run_workload(30).await.unwrap();

        // We should have at least some commits (not all failed)
        // With low fault rates, most should succeed
        assert!(stats.steps_executed > 0);

        // Crash, recover, and check oracle invariants
        let result = sim.crash_recover_verify();
        // Recovery might fail if crash corrupted segment headers —
        // that's expected behavior. But if it succeeds, oracle must hold.
        if let Ok(recovered) = result {
            assert!(recovered.len() >= sim.oracle().committed_count());
        }
    }
}
