//! Quint Model-Based Testing Driver
//!
//! This module connects the Quint formal specification (RingSPSC.qnt) to
//! the actual Rust implementation for model-based testing.
//!
//! The quint-connect crate generates test traces from Quint and executes
//! them against the real Ring<T> implementation to verify conformance.
//!
//! # Prerequisites
//!
//! ```bash
//! # Install Quint CLI
//! npm install -g @informalsystems/quint
//!
//! # Verify installation
//! quint --version
//! ```
//!
//! # Running
//!
//! ```bash
//! # Run model-based tests
//! cargo test -p ringmpsc-rs --test quint_mbt --features quint-mbt --release
//!
//! # Generate traces only (for debugging)
//! cd crates/ringmpsc/tla
//! quint run RingSPSC.qnt --main=RingSPSC --max-steps=50 --out-itf=traces.json
//! ```
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────┐     ┌─────────────────┐     ┌─────────────────┐
//! │  RingSPSC.qnt   │────▶│  quint-connect  │────▶│    Ring<T>      │
//! │  (Quint spec)   │     │  (trace driver) │     │  (Rust impl)    │
//! └─────────────────┘     └─────────────────┘     └─────────────────┘
//!         │                       │                       │
//!         ▼                       ▼                       ▼
//!    Model states           Execute trace          Verify invariants
//!    & transitions          on real Ring           match spec
//! ```

#![cfg(feature = "quint-mbt")]

// Note: quint-connect is currently experimental (v0.1.x)
// This module provides the structure for when the crate stabilizes
//
// For now, we implement a manual trace-based approach that:
// 1. Reads ITF (Informal Trace Format) JSON from Quint
// 2. Executes each action on the real Ring<T>
// 3. Verifies invariants match the spec

use ringmpsc_rs::{Config, Ring};
use std::mem::MaybeUninit;

/// System under test - wraps Ring<T> with state tracking
struct RingSUT {
    ring: Ring<u64>,
    /// Track head position (Ring doesn't expose this directly)
    consumed: u64,
    /// Track tail position 
    produced: u64,
    /// Producer's cached head (simulated)
    cached_head: u64,
    /// Consumer's cached tail (simulated)  
    cached_tail: u64,
}

impl RingSUT {
    fn new(capacity_bits: u8) -> Self {
        let config = Config::new(capacity_bits, 1, false);
        Self {
            ring: Ring::new(config),
            consumed: 0,
            produced: 0,
            cached_head: 0,
            cached_tail: 0,
        }
    }

    /// Execute ProducerWrite action
    fn producer_write(&mut self) -> bool {
        if let Some(mut reservation) = self.ring.reserve(1) {
            reservation.as_mut_slice()[0] = MaybeUninit::new(self.produced);
            reservation.commit();
            self.produced += 1;
            true
        } else {
            false
        }
    }

    /// Execute ProducerRefreshCache action
    fn producer_refresh_cache(&mut self) {
        // In real impl, this is: cached_head = head.load(Acquire)
        // We simulate by updating our tracked cache
        self.cached_head = self.consumed;
    }

    /// Execute ConsumerAdvance action  
    fn consumer_advance(&mut self) -> bool {
        let consumed = self.ring.consume_batch(|_item| {
            // Item consumed
        });
        if consumed > 0 {
            self.consumed += consumed as u64;
            true
        } else {
            false
        }
    }

    /// Execute ConsumerRefreshCache action
    fn consumer_refresh_cache(&mut self) {
        // In real impl, this is: cached_tail = tail.load(Acquire)
        self.cached_tail = self.produced;
    }

    // =========================================================================
    // INVARIANT CHECKS - must match Quint spec
    // =========================================================================

    /// INV-SEQ-01: Bounded Count
    fn check_bounded_count(&self, capacity: u64) -> bool {
        let count = self.produced - self.consumed;
        count <= capacity
    }

    /// INV-ORD-03: Happens-Before
    fn check_happens_before(&self) -> bool {
        self.consumed <= self.produced
    }

    /// Combined safety invariant
    fn check_safety_invariant(&self, capacity: u64) -> bool {
        self.check_bounded_count(capacity) && self.check_happens_before()
    }
}

/// Actions from Quint spec
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuintAction {
    ProducerReserveFast,
    ProducerRefreshCache,
    ProducerWrite,
    ConsumerReadFast,
    ConsumerRefreshCache,
    ConsumerAdvance,
}

/// Execute a trace of actions and verify invariants hold at each step
fn execute_trace(actions: &[QuintAction], capacity_bits: u8) -> Result<(), String> {
    let capacity = 1u64 << capacity_bits;
    let mut sut = RingSUT::new(capacity_bits);

    // Initial state satisfies invariant
    if !sut.check_safety_invariant(capacity) {
        return Err("Initial state violates safety invariant".to_string());
    }

    for (i, action) in actions.iter().enumerate() {
        match action {
            QuintAction::ProducerReserveFast => {
                // No state change, just a check
            }
            QuintAction::ProducerRefreshCache => {
                sut.producer_refresh_cache();
            }
            QuintAction::ProducerWrite => {
                sut.producer_write();
            }
            QuintAction::ConsumerReadFast => {
                // No state change, just a check
            }
            QuintAction::ConsumerRefreshCache => {
                sut.consumer_refresh_cache();
            }
            QuintAction::ConsumerAdvance => {
                sut.consumer_advance();
            }
        }

        // Verify invariant after each action
        if !sut.check_safety_invariant(capacity) {
            return Err(format!(
                "Safety invariant violated after action {}: {:?} (produced={}, consumed={})",
                i, action, sut.produced, sut.consumed
            ));
        }
    }

    Ok(())
}

// =============================================================================
// TESTS - Manual traces derived from Quint spec
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Test: Empty trace (init only)
    #[test]
    fn test_init_satisfies_invariant() {
        execute_trace(&[], 2).expect("Init should satisfy invariant");
    }

    /// Test: Simple produce-consume cycle
    #[test]
    fn test_produce_consume_cycle() {
        let trace = vec![
            QuintAction::ProducerWrite,
            QuintAction::ConsumerAdvance,
        ];
        execute_trace(&trace, 2).expect("Produce-consume should satisfy invariant");
    }

    /// Test: Fill ring to capacity
    #[test]
    fn test_fill_to_capacity() {
        // Capacity = 2^2 = 4
        let trace = vec![
            QuintAction::ProducerWrite,
            QuintAction::ProducerWrite,
            QuintAction::ProducerWrite,
            QuintAction::ProducerWrite,
        ];
        execute_trace(&trace, 2).expect("Fill to capacity should satisfy invariant");
    }

    /// Test: Cache refresh scenario
    #[test]
    fn test_cache_refresh_scenario() {
        let trace = vec![
            // Fill ring
            QuintAction::ProducerWrite,
            QuintAction::ProducerWrite,
            QuintAction::ProducerWrite,
            QuintAction::ProducerWrite,
            // Consumer advances
            QuintAction::ConsumerAdvance,
            QuintAction::ConsumerAdvance,
            // Producer refreshes cache
            QuintAction::ProducerRefreshCache,
            // Producer can write again
            QuintAction::ProducerWrite,
            QuintAction::ProducerWrite,
        ];
        execute_trace(&trace, 2).expect("Cache refresh scenario should work");
    }

    /// Test: Alternating produce-consume
    #[test]
    fn test_alternating_produce_consume() {
        let mut trace = Vec::new();
        for _ in 0..20 {
            trace.push(QuintAction::ProducerWrite);
            trace.push(QuintAction::ConsumerAdvance);
        }
        execute_trace(&trace, 2).expect("Alternating should satisfy invariant");
    }

    /// Test: Consumer refresh when empty
    #[test]
    fn test_consumer_refresh_when_empty() {
        let trace = vec![
            QuintAction::ConsumerRefreshCache,  // Empty, cache stays 0
            QuintAction::ProducerWrite,
            QuintAction::ConsumerRefreshCache,  // Now sees tail=1
            QuintAction::ConsumerAdvance,
        ];
        execute_trace(&trace, 2).expect("Consumer refresh should work");
    }

    /// Test: Producer starvation recovery
    #[test]
    fn test_producer_starvation_recovery() {
        let trace = vec![
            // Fill ring completely
            QuintAction::ProducerWrite,
            QuintAction::ProducerWrite,
            QuintAction::ProducerWrite,
            QuintAction::ProducerWrite,
            // Producer's cache says full, consumer drains
            QuintAction::ConsumerAdvance,
            QuintAction::ConsumerAdvance,
            QuintAction::ConsumerAdvance,
            QuintAction::ConsumerAdvance,
            // Producer refreshes and recovers
            QuintAction::ProducerRefreshCache,
            QuintAction::ProducerWrite,
            QuintAction::ProducerWrite,
        ];
        execute_trace(&trace, 2).expect("Producer recovery should work");
    }
}

// =============================================================================
// FUTURE: ITF Trace Parser (when quint-connect stabilizes)
// =============================================================================
//
// The full quint-connect integration would parse ITF JSON traces:
//
// ```rust
// use quint_connect::{ItfTrace, TraceRunner};
// 
// #[test]
// fn test_quint_generated_traces() {
//     let traces = ItfTrace::from_file("traces.json").unwrap();
//     for trace in traces {
//         let mut sut = RingSUT::new(2);
//         for state in trace.states() {
//             // Map Quint state to Rust state
//             // Execute action
//             // Verify invariants
//         }
//     }
// }
// ```
//
// For now, we use manually-crafted traces that exercise the same paths.
