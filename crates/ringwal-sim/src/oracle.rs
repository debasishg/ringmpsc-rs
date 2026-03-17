//! Ground-truth oracle for verifying WAL recovery correctness.
//!
//! The `CommitOracle` records every acknowledged commit and its entries.
//! After a crash-and-recover cycle, `verify_against()` checks that every
//! committed transaction appears in the recovery output (no lost commits).

use std::collections::HashMap;

use ringwal::{RecoveredTransaction, RecoveryAction};

/// A record of one acknowledged commit, stored by the oracle as ground truth.
#[derive(Debug, Clone)]
pub struct CommittedTx<K, V> {
    pub tx_id: u64,
    pub entries: Vec<(K, V)>,
}

/// Ground-truth tracker for acknowledged WAL commits.
///
/// Call `record_commit()` each time a `WalWriter::commit()` returns `Ok`.
/// After crash + recovery, call `verify_against()` to assert that no
/// acknowledged commit was lost.
#[derive(Debug)]
pub struct CommitOracle<K, V> {
    /// `tx_id` → committed entries (ground truth).
    committed: HashMap<u64, CommittedTx<K, V>>,
}

impl<K, V> CommitOracle<K, V> {
    /// Creates an empty oracle.
    #[must_use] 
    pub fn new() -> Self {
        Self {
            committed: HashMap::new(),
        }
    }

    /// Records that `tx_id` committed with the given key-value entries.
    ///
    /// Should be called only when `WalWriter::commit(tx_id).await` returns `Ok`.
    pub fn record_commit(&mut self, tx_id: u64, entries: Vec<(K, V)>) {
        self.committed.insert(tx_id, CommittedTx { tx_id, entries });
    }

    /// Returns the number of acknowledged commits.
    #[must_use] 
    pub fn committed_count(&self) -> usize {
        self.committed.len()
    }

    /// Returns iterator over committed `tx_ids`.
    pub fn committed_tx_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.committed.keys().copied()
    }

    /// Resets the oracle (e.g., after a full WAL truncate).
    pub fn clear(&mut self) {
        self.committed.clear();
    }
}

impl<K, V> Default for CommitOracle<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

/// Verification result returned by `verify_recovery`.
#[derive(Debug)]
pub struct VerificationResult {
    /// Number of oracle-committed txns found in recovery.
    pub found: usize,
    /// `tx_ids` of oracle-committed txns NOT found in recovery (bugs!).
    pub lost: Vec<u64>,
    /// `tx_ids` recovered as committed but NOT in oracle (phantom commits).
    pub phantom: Vec<u64>,
    /// Total recovered committed transactions.
    pub recovered_committed: usize,
}

impl VerificationResult {
    /// Returns `true` if no anomalies were detected.
    #[must_use] 
    pub fn is_ok(&self) -> bool {
        self.lost.is_empty() && self.phantom.is_empty()
    }
}

/// Verifies recovery output against oracle ground truth.
///
/// Checks two properties:
/// 1. **No lost commits** (INV-WAL-05): every oracle-committed tx appears as
///    `RecoveryAction::Commit` in the recovered set.
/// 2. **No phantom commits** (INV-WAL-07): no recovered `Commit` was _not_
///    acknowledged by the oracle.
#[must_use] 
pub fn verify_recovery<K, V>(
    oracle: &CommitOracle<K, V>,
    recovered: &[RecoveredTransaction<K, V>],
) -> VerificationResult {
    let recovered_committed: HashMap<u64, &RecoveredTransaction<K, V>> = recovered
        .iter()
        .filter(|tx| tx.action == RecoveryAction::Commit)
        .map(|tx| (tx.tx_id, tx))
        .collect();

    let mut found = 0;
    let mut lost = Vec::new();

    for &tx_id in oracle.committed.keys() {
        if recovered_committed.contains_key(&tx_id) {
            found += 1;
        } else {
            lost.push(tx_id);
        }
    }

    let mut phantom = Vec::new();
    for &tx_id in recovered_committed.keys() {
        if !oracle.committed.contains_key(&tx_id) {
            phantom.push(tx_id);
        }
    }

    lost.sort_unstable();
    phantom.sort_unstable();

    VerificationResult {
        found,
        lost,
        phantom,
        recovered_committed: recovered_committed.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringwal::{RecoveredTransaction, RecoveryAction, WalEntry};

    #[test]
    fn empty_oracle_empty_recovery() {
        let oracle: CommitOracle<String, Vec<u8>> = CommitOracle::new();
        let recovered: Vec<RecoveredTransaction<String, Vec<u8>>> = vec![];
        let result = verify_recovery(&oracle, &recovered);
        assert!(result.is_ok());
        assert_eq!(result.found, 0);
        assert_eq!(result.recovered_committed, 0);
    }

    #[test]
    fn oracle_matches_recovery() {
        let mut oracle: CommitOracle<String, Vec<u8>> = CommitOracle::new();
        oracle.record_commit(1, vec![("k1".into(), b"v1".to_vec())]);
        oracle.record_commit(2, vec![("k2".into(), b"v2".to_vec())]);

        let recovered = vec![
            RecoveredTransaction {
                tx_id: 1,
                action: RecoveryAction::Commit,
                entries: vec![WalEntry::Insert {
                    tx_id: 1,
                    timestamp: 100,
                    key: "k1".into(),
                    value: b"v1".to_vec(),
                }],
            },
            RecoveredTransaction {
                tx_id: 2,
                action: RecoveryAction::Commit,
                entries: vec![WalEntry::Insert {
                    tx_id: 2,
                    timestamp: 101,
                    key: "k2".into(),
                    value: b"v2".to_vec(),
                }],
            },
        ];

        let result = verify_recovery(&oracle, &recovered);
        assert!(result.is_ok());
        assert_eq!(result.found, 2);
    }

    #[test]
    fn detects_lost_commit() {
        let mut oracle: CommitOracle<String, Vec<u8>> = CommitOracle::new();
        oracle.record_commit(1, vec![("k1".into(), b"v1".to_vec())]);
        oracle.record_commit(2, vec![("k2".into(), b"v2".to_vec())]);

        // Only tx 1 recovered as committed
        let recovered = vec![RecoveredTransaction {
            tx_id: 1,
            action: RecoveryAction::Commit,
            entries: vec![],
        }];

        let result = verify_recovery(&oracle, &recovered);
        assert!(!result.is_ok());
        assert_eq!(result.lost, vec![2]);
    }

    #[test]
    fn detects_phantom_commit() {
        let oracle: CommitOracle<String, Vec<u8>> = CommitOracle::new();

        // A tx recovered as committed but never acknowledged
        let recovered = vec![RecoveredTransaction {
            tx_id: 99,
            action: RecoveryAction::Commit,
            entries: vec![],
        }];

        let result = verify_recovery(&oracle, &recovered);
        assert!(!result.is_ok());
        assert_eq!(result.phantom, vec![99]);
    }

    #[test]
    fn incomplete_txns_ignored() {
        let mut oracle: CommitOracle<String, Vec<u8>> = CommitOracle::new();
        oracle.record_commit(1, vec![("k".into(), b"v".to_vec())]);

        // tx 1 committed, tx 2 incomplete — oracle has no record of tx 2
        let recovered = vec![
            RecoveredTransaction {
                tx_id: 1,
                action: RecoveryAction::Commit,
                entries: vec![],
            },
            RecoveredTransaction {
                tx_id: 2,
                action: RecoveryAction::Incomplete,
                entries: vec![],
            },
        ];

        let result = verify_recovery(&oracle, &recovered);
        assert!(result.is_ok());
        assert_eq!(result.found, 1);
    }
}
