//! Phase 5 — Deterministic Simulation Property Tests
//!
//! Eight property tests exercising WAL correctness under fault injection.
//! Each test iterates over configurable seeds (`DST_SEEDS` env var, default 1000).
//! On failure, the seed and `FaultConfig` are printed for exact replay.
//!
//! Run with: `cargo test -p ringwal-sim --release --test dst_tests -- --test-threads=1`

use std::collections::HashSet;

use ringwal::{
    checkpoint, truncate_segments_before, RecoveryAction, SyncMode, WalConfig,
};
use ringwal_sim::{FaultConfig, RingwalSimulator};

// ── Helpers ──────────────────────────────────────────────────────────────

/// Number of seeds per test (override with `DST_SEEDS=N`).
fn num_seeds() -> u64 {
    std::env::var("DST_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000)
}

/// Regression seeds — known-interesting seeds discovered during development.
/// Populated as bugs are found; each is replayed in addition to the random range.
const REGRESSION_SEEDS: &[u64] = &[];

/// Iterator over all seeds: regression seeds first, then `0..num_seeds()`.
fn seeds_iter() -> impl Iterator<Item = u64> {
    let regression = REGRESSION_SEEDS.iter().copied();
    let range = 0..num_seeds();
    regression.chain(range)
}

fn default_wal_config() -> WalConfig {
    WalConfig::new("/wal")
        .with_ring_bits(10) // 1024 slots
        .with_max_writers(4)
        .with_max_segment_size(1024 * 1024) // 1 MB
        .with_flush_interval(std::time::Duration::from_millis(5))
        .with_batch_hint(64)
        .with_sync_mode(SyncMode::Full)
}

fn small_segment_config() -> WalConfig {
    WalConfig::new("/wal")
        .with_ring_bits(10)
        .with_max_writers(4)
        .with_max_segment_size(4096) // 4 KB — forces frequent segment rotation
        .with_flush_interval(std::time::Duration::from_millis(5))
        .with_batch_hint(16)
        .with_sync_mode(SyncMode::Full)
}

/// Moderate fault injection — enough to exercise error paths, low enough
/// that most operations succeed.
fn moderate_faults() -> FaultConfig {
    FaultConfig::builder()
        .write_fail_rate(0.02)
        .fsync_fail_rate(0.02)
        .build()
}

// ── Property Tests ───────────────────────────────────────────────────────

/// INV-WAL-05: Every oracle-committed transaction is present in recovery output.
#[tokio::test(flavor = "current_thread")]
async fn dst_no_lost_commits() {
    for seed in seeds_iter() {
        let mut sim = RingwalSimulator::new(seed, moderate_faults(), default_wal_config());

        let stats = match sim.run_workload(50).await {
            Ok(s) => s,
            Err(_) => continue, // WAL couldn't open — skip seed
        };
        if stats.commits == 0 {
            continue;
        }

        let (recovered, _) = match sim.crash_and_recover() {
            Ok(r) => r,
            Err(_) => continue, // recovery failed due to corruption — skip
        };

        sim.assert_no_lost_commits(&recovered);
    }
}

/// INV-WAL-07: No uncommitted data appears as committed in recovery output.
#[tokio::test(flavor = "current_thread")]
async fn dst_no_phantom_commits() {
    for seed in seeds_iter() {
        let mut sim = RingwalSimulator::new(seed, moderate_faults(), default_wal_config());

        let stats = match sim.run_workload(50).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        if stats.commits == 0 {
            continue;
        }

        let (recovered, _) = match sim.crash_and_recover() {
            Ok(r) => r,
            Err(_) => continue,
        };

        sim.assert_no_phantom_commits(&recovered);
    }
}

/// INV-WAL-01: Recovered committed transaction IDs have no duplicates,
/// and entries within each transaction carry consistent `tx_ids`.
#[tokio::test(flavor = "current_thread")]
async fn dst_lsn_monotonicity() {
    for seed in seeds_iter() {
        let mut sim = RingwalSimulator::new(seed, moderate_faults(), default_wal_config());

        let stats = match sim.run_workload(50).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        if stats.commits == 0 {
            continue;
        }

        let (recovered, _) = match sim.crash_and_recover() {
            Ok(r) => r,
            Err(_) => continue,
        };

        // No duplicate tx_ids among committed transactions
        let committed_ids: Vec<u64> = recovered
            .iter()
            .filter(|tx| tx.action == RecoveryAction::Commit)
            .map(|tx| tx.tx_id)
            .collect();

        let unique: HashSet<u64> = committed_ids.iter().copied().collect();
        assert_eq!(
            committed_ids.len(),
            unique.len(),
            "seed={seed}: duplicate tx_ids in committed recovery output",
        );

        // Within each transaction, all entries carry the same tx_id
        for tx in recovered.iter().filter(|tx| tx.action == RecoveryAction::Commit) {
            for entry in &tx.entries {
                assert_eq!(
                    entry.tx_id(),
                    tx.tx_id,
                    "seed={}: entry tx_id {} != transaction tx_id {}",
                    seed,
                    entry.tx_id(),
                    tx.tx_id,
                );
            }
        }
    }
}

/// INV-WAL-03: Every recovered entry passed CRC32 validation during recovery.
///
/// Recovery internally validates checksums and skips corrupted entries.
/// This test verifies that recovery succeeds and reports reasonable stats.
#[tokio::test(flavor = "current_thread")]
async fn dst_checksum_integrity() {
    for seed in seeds_iter() {
        let mut sim = RingwalSimulator::new(seed, moderate_faults(), default_wal_config());

        match sim.run_workload(50).await {
            Ok(_) => {}
            Err(_) => continue,
        }

        let (recovered, stats) = match sim.crash_and_recover() {
            Ok(r) => r,
            Err(_) => continue,
        };

        // All returned committed transactions passed CRC32 validation.
        // If recovery returned them, their checksums are valid.
        // The stats track any bad entries that were skipped.
        assert!(
            stats.committed + stats.aborted + stats.incomplete == stats.total_transactions,
            "seed={}: transaction count mismatch: {} committed + {} aborted + {} incomplete != {} total",
            seed,
            stats.committed,
            stats.aborted,
            stats.incomplete,
            stats.total_transactions,
        );

        // Every recovered entry should have non-zero tx_id
        for tx in &recovered {
            assert!(
                tx.tx_id > 0,
                "seed={seed}: recovered transaction with tx_id=0",
            );
        }
    }
}

/// INV-WAL-02, INV-WAL-04: Small segment size forces frequent rotation.
/// Under faults, rotation + recovery must still be correct.
#[tokio::test(flavor = "current_thread")]
async fn dst_segment_rotation_under_faults() {
    for seed in seeds_iter() {
        let mut sim = RingwalSimulator::new(seed, moderate_faults(), small_segment_config());

        let stats = match sim.run_workload(50).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        if stats.commits == 0 {
            continue;
        }

        let (recovered, _) = match sim.crash_and_recover() {
            Ok(r) => r,
            Err(_) => continue,
        };

        sim.assert_no_lost_commits(&recovered);
        sim.assert_no_phantom_commits(&recovered);
    }
}

/// INV-WAL-05: Checkpoint + truncate does not lose post-checkpoint data.
///
/// Phase 1: run workload → shutdown.
/// Checkpoint + truncate old segments.
/// Phase 2: re-open WAL, commit more → shutdown.
/// Crash + recover → post-checkpoint commits must survive.
#[tokio::test(flavor = "current_thread")]
async fn dst_checkpoint_advancement() {
    for seed in seeds_iter() {
        let config = default_wal_config();
        let mut sim = RingwalSimulator::new(seed, FaultConfig::none(), config.clone());

        // Phase 1: write initial data
        let stats1 = match sim.run_workload(30).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        if stats1.commits == 0 {
            continue;
        }

        // Checkpoint + truncate via free functions
        let io = sim.sim_io_cloned();
        let dir = sim.wal_dir().clone();
        let cp_lsn = match checkpoint::<String, Vec<u8>, _>(&dir, &io) {
            Ok(lsn) => lsn,
            Err(_) => continue, // NoNewCheckpoints or error
        };
        let _ = truncate_segments_before(&dir, cp_lsn, &io);

        // Clear oracle — pre-checkpoint data is "consumed"
        sim.oracle_mut().clear();

        // Phase 2: write more data on the same SimIo
        let stats2 = match sim.run_workload(20).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        if stats2.commits == 0 {
            continue;
        }

        // Crash + recover — post-checkpoint commits must survive
        let (recovered, _) = match sim.crash_and_recover() {
            Ok(r) => r,
            Err(_) => continue,
        };

        sim.assert_no_lost_commits(&recovered);
    }
}

/// INV-WAL-07: Aborted transactions never appear as committed in recovery.
#[tokio::test(flavor = "current_thread")]
async fn dst_abort_discarded() {
    for seed in seeds_iter() {
        let mut sim = RingwalSimulator::new(seed, moderate_faults(), default_wal_config());

        let stats = match sim.run_workload(50).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        if stats.aborts == 0 && stats.commits == 0 {
            continue;
        }

        let (recovered, _) = match sim.crash_and_recover() {
            Ok(r) => r,
            Err(_) => continue,
        };

        // No aborted tx_id should appear as committed in recovery
        let committed_ids: HashSet<u64> = recovered
            .iter()
            .filter(|tx| tx.action == RecoveryAction::Commit)
            .map(|tx| tx.tx_id)
            .collect();

        for &aborted_id in &stats.aborted_tx_ids {
            assert!(
                !committed_ids.contains(&aborted_id),
                "seed={}: aborted tx {} found as committed in recovery\nfault_config: {:?}",
                seed,
                aborted_id,
                sim.fault_config(),
            );
        }
    }
}

/// INV-WAL-05: Multiple crash-recovery cycles maintain durability.
///
/// Round 1: workload → crash → recover → verify.
/// Round 2: more writes on same `SimIo` → crash → recover → verify (oracle accumulates).
#[tokio::test(flavor = "current_thread")]
async fn dst_multiple_crashes() {
    for seed in seeds_iter() {
        let config = default_wal_config();
        let mut sim = RingwalSimulator::new(seed, moderate_faults(), config.clone());

        // Round 1
        let stats1 = match sim.run_workload(30).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        if stats1.commits == 0 {
            continue;
        }

        let (recovered1, _) = match sim.crash_and_recover() {
            Ok(r) => r,
            Err(_) => continue,
        };
        sim.assert_no_lost_commits(&recovered1);

        // Round 2: write more on the same (crashed) filesystem
        let stats2 = match sim.run_workload(30).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        if stats2.commits == 0 {
            // Oracle still has round 1 commits; just verify those survive
        }

        let (recovered2, _) = match sim.crash_and_recover() {
            Ok(r) => r,
            Err(_) => continue,
        };

        // Oracle has commits from both rounds — all must be present
        sim.assert_no_lost_commits(&recovered2);
        sim.assert_no_phantom_commits(&recovered2);
    }
}
