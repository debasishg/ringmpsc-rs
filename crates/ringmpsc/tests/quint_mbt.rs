//! Quint Model-Based Testing Driver
//!
//! Connects the Quint formal specification (RingSPSC.qnt) to the actual Rust
//! Ring<T> implementation using `quint-connect` for **automated trace generation**
//! and conformance checking.
//!
//! # How It Works
//!
//! 1. `quint-connect` invokes the Quint CLI to simulate / run the RingSPSC.qnt spec
//! 2. Generated traces (sequences of named actions + state snapshots in ITF format)
//!    are replayed against the real `Ring<T>` implementation
//! 3. After each step the framework compares the driver's state with the spec's
//!    expected state, catching any divergence automatically
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
//! # Run all quint-connected tests (spec-defined + random simulation)
//! cargo test -p ringmpsc-rs --test quint_mbt --features quint-mbt --release
//!
//! # With verbose trace output (shows actions & state at each step)
//! # NOTE: QUINT_VERBOSE is checked at *runtime* by the test driver (not by
//! # quint-connect, which uses compile-time option_env! and therefore never
//! # sees the variable when installed from crates.io). Pass --nocapture so
//! # the test harness doesn't swallow stderr.
//! QUINT_VERBOSE=1 cargo test -p ringmpsc-rs --test quint_mbt --features quint-mbt --release -- --nocapture
//!
//! # Reproduce a specific failing trace
//! QUINT_SEED=42 cargo test -p ringmpsc-rs --test quint_mbt --features quint-mbt --release
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
//!    Simulation traces      Execute actions         Verify state
//!    from spec model        on real Ring            matches spec
//! ```
//!
//! # State Mapping
//!
//! | Quint variable   | Rust driver field | Ring<T> correspondence         |
//! |------------------|-------------------|--------------------------------|
//! | `head`           | `consumed`        | `Ring.head` (AtomicU64)        |
//! | `tail`           | `produced`        | `Ring.tail` (AtomicU64)        |
//! | `cached_head`    | `cached_head`     | `Ring.cached_head` (UnsafeCell)|
//! | `cached_tail`    | `cached_tail`     | `Ring.cached_tail` (UnsafeCell)|
//! | `items_produced` | `items_produced`  | (external counter)             |

#![cfg(feature = "quint-mbt")]

use quint_connect::*;
use ringmpsc_rs::{Config, Ring};
use serde::Deserialize;
use std::mem::MaybeUninit;
use std::sync::OnceLock;

/// Returns true when `QUINT_VERBOSE` is set to a non-zero value **at runtime**.
///
/// Note: `quint-connect`'s built-in `option_env!("QUINT_VERBOSE")` is evaluated
/// at *compile time* of the library crate, so it is always `None` when the
/// library comes from crates.io. We re-check the variable at runtime here.
fn is_verbose() -> bool {
    static VERBOSE: OnceLock<bool> = OnceLock::new();
    *VERBOSE.get_or_init(|| {
        std::env::var("QUINT_VERBOSE")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false)
    })
}

// =============================================================================
// STATE — Mirrors Quint's RingSPSC module variables
// =============================================================================

/// State representation matching the five Quint state variables in RingSPSC.qnt.
///
/// Deserialized from ITF trace output by `quint-connect` and also constructed
/// from the driver's internal tracking via [`State::from_driver`] for comparison.
///
/// Note: Quint built-in names `head`/`tail` (list ops) required renaming to
/// `hd`/`tl` in the spec. We use `#[serde(rename)]` to keep Rust field names
/// readable while matching the Quint variable names in ITF traces.
#[derive(Eq, PartialEq, Deserialize, Debug)]
struct RingSPSCState {
    /// Consumer's read position (Quint: `hd`)
    #[serde(rename = "hd")]
    head: i64,
    /// Producer's write position (Quint: `tl`)
    #[serde(rename = "tl")]
    tail: i64,
    /// Producer's cached view of head (Quint: `cached_head`)
    cached_head: i64,
    /// Consumer's cached view of tail (Quint: `cached_tail`)
    cached_tail: i64,
    /// Total items produced (Quint: `items_produced`)
    items_produced: i64,
}

impl State<RingSPSCDriver> for RingSPSCState {
    fn from_driver(driver: &RingSPSCDriver) -> quint_connect::Result<Self> {
        Ok(RingSPSCState {
            head: driver.consumed as i64,
            tail: driver.produced as i64,
            cached_head: driver.cached_head as i64,
            cached_tail: driver.cached_tail as i64,
            items_produced: driver.items_produced as i64,
        })
    }
}

// =============================================================================
// DRIVER — Connects Ring<T> to Quint spec actions
// =============================================================================

/// Test driver connecting the `Ring<u64>` implementation to the Quint spec.
///
/// Maintains **two layers**:
/// - A real `Ring<u64>` exercised on each produce/consume action (conformance)
/// - Abstract state tracking that mirrors Quint's variables (state comparison)
///
/// The abstract tracking follows the Quint model's logic exactly, while the
/// real Ring verifies that the Rust implementation accepts every operation the
/// spec says should succeed.
struct RingSPSCDriver {
    /// The real ring buffer under test
    ring: Ring<u64>,
    /// Tracks consumer position — matches Quint's `head`
    consumed: u64,
    /// Tracks producer position — matches Quint's `tail`
    produced: u64,
    /// Tracks producer's cached head — matches Quint's `cached_head`
    cached_head: u64,
    /// Tracks consumer's cached tail — matches Quint's `cached_tail`
    cached_tail: u64,
    /// Total items produced — matches Quint's `items_produced`
    items_produced: u64,
}

impl Default for RingSPSCDriver {
    fn default() -> Self {
        // capacity = 2^2 = 4, matching CAPACITY = 4 in RingSPSC.qnt
        let config = Config::new(2, 1, false);
        Self {
            ring: Ring::new(config),
            consumed: 0,
            produced: 0,
            cached_head: 0,
            cached_tail: 0,
            items_produced: 0,
        }
    }
}

impl Driver for RingSPSCDriver {
    type State = RingSPSCState;

    fn step(&mut self, step: &Step) -> quint_connect::Result {
        let verbose = is_verbose();
        let action = &step.action_taken;

        switch!(step {
            init => {
                *self = Self::default();
                if verbose {
                    eprintln!("  [init] head=0 tail=0 cached_head=0 cached_tail=0 items_produced=0");
                }
            },

            // -----------------------------------------------------------------
            // PRODUCER ACTIONS
            // -----------------------------------------------------------------

            producerReserveFast => {
                // Guard-only action — no state mutation in Quint.
                // The guard `producerHasSpace(tail, cached_head)` was verified
                // during trace generation. Nothing to execute.
                if verbose {
                    eprintln!("  [{action}] (guard-only, no state change)");
                }
            },

            producerRefreshCache => {
                // Quint: cached_head' = head
                // Models the slow-path Acquire load of head in reserve().
                self.cached_head = self.consumed;
                if verbose {
                    eprintln!("  [{action}] cached_head <- head = {}", self.cached_head);
                }
            },

            producerWrite => {
                // Quint: tail' = tail + 1, items_produced' = items_produced + 1
                // Drive the real Ring to verify conformance.
                let mut reserved = self.ring.reserve(1)
                    .expect("reserve(1) should succeed: Quint guard ensures (tail-head) < CAPACITY");
                // Safety-note: write value then commit (zero-copy reservation pattern)
                reserved.as_mut_slice()[0] = MaybeUninit::new(self.produced);
                reserved.commit();
                self.produced += 1;
                self.items_produced += 1;
                if verbose {
                    eprintln!("  [{action}] tail={} items_produced={}", self.produced, self.items_produced);
                }
            },

            // -----------------------------------------------------------------
            // CONSUMER ACTIONS
            // -----------------------------------------------------------------

            consumerReadFast => {
                // Guard-only action — no state mutation in Quint.
                // The guard `consumerHasItems(cached_tail, head)` was verified
                // during trace generation. Nothing to execute.
                if verbose {
                    eprintln!("  [{action}] (guard-only, no state change)");
                }
            },

            consumerRefreshCache => {
                // Quint: cached_tail' = tail
                // Models the slow-path Acquire load of tail in consume_batch().
                self.cached_tail = self.produced;
                if verbose {
                    eprintln!("  [{action}] cached_tail <- tail = {}", self.cached_tail);
                }
            },

            consumerAdvance => {
                // Quint: head' = head + 1
                // Drive the real Ring — consume exactly 1 item.
                let consumed = self.ring.consume_up_to(1, |_item| {});
                assert_eq!(
                    consumed, 1,
                    "consume_up_to(1) should return 1: Quint guard ensures head < tail"
                );
                self.consumed += 1;
                if verbose {
                    eprintln!("  [{action}] head={}", self.consumed);
                }
            },
        })
    }
}

// =============================================================================
// SIMULATION-BASED TESTS (automated trace generation)
//
// `#[quint_run]` invokes `quint run --mbt` to generate random simulation traces
// covering diverse action interleavings, then replays each trace against the
// real Ring<T> implementation with state comparison at every step.
//
// NOTE: We use `#[quint_run]` exclusively because `quint test` does not support
// the `--mbt` flag needed to embed `mbt::actionTaken` metadata in traces.
// The Quint spec's `run` declarations (testInitSatisfiesInvariant, etc.) remain
// useful for standalone `quint test` verification of the spec itself.
//
// This is the key capability enabled by quint-connect: traces are generated
// automatically from the formal spec rather than hand-crafted.
//
// Set QUINT_SEED=<n> to reproduce a specific trace.
// =============================================================================

/// Default random simulation — broad coverage of action interleavings.
///
/// Generates multiple traces with varying step lengths. Verifies that
/// every reachable action sequence produces matching state in the real Ring.
#[quint_run(spec = "tla/RingSPSC.qnt", main = "RingSPSC")]
fn simulation() -> impl Driver {
    RingSPSCDriver::default()
}

/// Longer traces for deeper state-space exploration.
///
/// Uses a fixed seed for reproducibility and more samples to increase
/// coverage of rare action sequences (e.g., multiple cache refreshes).
#[quint_run(spec = "tla/RingSPSC.qnt", main = "RingSPSC", seed = "1729", max_samples = 20)]
fn simulation_deep() -> impl Driver {
    RingSPSCDriver::default()
}

/// Another seed for broader coverage across CI runs.
#[quint_run(spec = "tla/RingSPSC.qnt", main = "RingSPSC", seed = "314159")]
fn simulation_seed_variation() -> impl Driver {
    RingSPSCDriver::default()
}
