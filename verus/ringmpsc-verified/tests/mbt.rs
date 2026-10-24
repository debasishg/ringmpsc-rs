//! Model-based testing of the **verified** ring against the Quint spec
//! (phase 7, #26).
//!
//! The same traces `crates/ringmpsc/tests/quint_mbt.rs` replays against the
//! production `Ring` are replayed here against the verified `spsc` halves:
//! `quint-connect` runs `quint run --mbt` on `RingSPSC.qnt`, and this driver
//! executes each step and compares its state with the spec's.
//!
//! One check goes beyond the production driver: on `consumerAdvance` the
//! value popped must be the next one pushed, so FIFO is checked on values,
//! not only on counts.
//!
//! Needs the Quint CLI (>= 0.31) on `PATH`:
//!
//! ```text
//! CARGO_TARGET_DIR=target/plain cargo test --test mbt
//! QUINT_SEED=<n> ...   # reproduce a run
//! ```


// Miri can't spawn the Quint CLI, and proptest's case volume is too slow
// under it; these run natively only.
#![cfg(not(miri))]
use quint_connect::*;
use ringmpsc_verified::spsc::{channel, Consumer, Producer};
use serde::Deserialize;
use std::collections::BTreeSet;

/// The spec, relative to this crate's directory (where `cargo test` runs).
const SPEC: &str = "../../crates/ringmpsc/tla/RingSPSC.qnt";

/// `CAPACITY = 4` in `RingSPSC.qnt`: `ring_bits = 2`.
const CAPACITY: u64 = 4;

fn runtime_seed() -> String {
    std::env::var("QUINT_SEED").unwrap_or_else(|_| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before epoch")
            .subsec_nanos();
        format!("0x{nanos:x}")
    })
}

/// The Quint state variables, as in the production driver.
#[derive(Eq, PartialEq, Deserialize, Debug)]
struct RingSPSCState {
    #[serde(rename = "hd")]
    head: i64,
    #[serde(rename = "tl")]
    tail: i64,
    cached_head: i64,
    cached_tail: i64,
    items_produced: i64,
    buffer_capacity: i64,
    initialized: BTreeSet<i64>,
    buffer_aligned: bool,
    allocator_zst: bool,
    numa_placement_valid: bool,
    numa_fallback_safe: bool,
    numa_policy_deterministic: bool,
}

impl State<VerifiedDriver> for RingSPSCState {
    fn from_driver(d: &VerifiedDriver) -> quint_connect::Result<Self> {
        Ok(RingSPSCState {
            head: d.consumed as i64,
            tail: d.produced as i64,
            cached_head: d.cached_head as i64,
            cached_tail: d.cached_tail as i64,
            items_produced: d.produced as i64,
            buffer_capacity: CAPACITY as i64,
            initialized: d.initialized.iter().map(|&i| i as i64).collect(),
            // Allocator and NUMA flags are modeled as constants in the spec;
            // the verified ring has no allocator or NUMA layer.
            buffer_aligned: true,
            allocator_zst: true,
            numa_placement_valid: true,
            numa_fallback_safe: true,
            numa_policy_deterministic: true,
        })
    }
}

/// Drives the verified producer and consumer halves, and tracks the spec's
/// abstract variables alongside.
struct VerifiedDriver {
    producer: Producer<u64>,
    consumer: Consumer<u64>,
    produced: u64,
    consumed: u64,
    cached_head: u64,
    cached_tail: u64,
    initialized: BTreeSet<u64>,
}

impl Default for VerifiedDriver {
    fn default() -> Self {
        let (producer, consumer) = channel::<u64>(2);
        Self {
            producer,
            consumer,
            produced: 0,
            consumed: 0,
            cached_head: 0,
            cached_tail: 0,
            initialized: BTreeSet::new(),
        }
    }
}

impl Driver for VerifiedDriver {
    type State = RingSPSCState;

    fn step(&mut self, step: &Step) -> quint_connect::Result {
        switch!(step {
            init => {
                *self = Self::default();
            },
            producerReserveFast => {
                // Guard-only in the spec: nothing to execute.
            },
            producerRefreshCache => {
                self.cached_head = self.consumed;
            },
            producerWrite => {
                // The spec's guard (tail - cached_head < CAPACITY) means the
                // real ring has room, so a verified push must succeed.
                let slot = self.produced % CAPACITY;
                assert!(
                    self.producer.push(self.produced).is_ok(),
                    "verified push refused, but the spec says there is room"
                );
                self.initialized.insert(slot);
                self.produced += 1;
            },
            consumerReadFast => {
                // Guard-only in the spec: nothing to execute.
            },
            consumerRefreshCache => {
                self.cached_tail = self.produced;
            },
            consumerAdvance => {
                // The spec's guard (head < tail) means an item is published.
                // It must be the next one pushed: FIFO on values.
                let slot = self.consumed % CAPACITY;
                assert_eq!(
                    self.consumer.pop(),
                    Some(self.consumed),
                    "verified pop returned the wrong item"
                );
                self.initialized.remove(&slot);
                self.consumed += 1;
            },
        })
    }
}

/// Random simulation traces, as the production `simulation` test.
#[test]
fn simulation() {
    let config = quint_connect::runner::Config {
        test_name: "simulation".to_string(),
        gen_config: quint_connect::runner::RunConfig {
            spec: SPEC.to_string(),
            main: Some("RingSPSC".to_string()),
            init: None,
            step: None,
            max_samples: None,
            max_steps: None,
            seed: runtime_seed(),
        },
    };
    if let Err(err) = quint_connect::runner::run_test(VerifiedDriver::default(), config) {
        panic!("{}", err);
    }
}

/// The production driver's fixed seeds, so both rings see the same traces.
#[quint_run(spec = "../../crates/ringmpsc/tla/RingSPSC.qnt", main = "RingSPSC", seed = "1729", max_samples = 20)]
fn simulation_deep() -> impl Driver {
    VerifiedDriver::default()
}

#[quint_run(spec = "../../crates/ringmpsc/tla/RingSPSC.qnt", main = "RingSPSC", seed = "314159")]
fn simulation_seed_variation() -> impl Driver {
    VerifiedDriver::default()
}
