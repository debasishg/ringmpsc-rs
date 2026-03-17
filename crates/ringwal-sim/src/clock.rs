//! Simulated clock for deterministic time control.
//!
//! Uses a `Cell<u64>` monotonic counter instead of `SystemTime::now()`.
//! Single-threaded by design — DST runs everything on one thread.

use std::cell::Cell;

/// A deterministic clock that returns seconds since epoch.
///
/// Starts at `initial_secs` (default `1_700_000_000` ≈ Nov 2023) and
/// advances only when explicitly told to. This makes timestamp-dependent
/// logic fully reproducible.
#[derive(Debug)]
pub struct SimClock {
    secs: Cell<u64>,
}

impl SimClock {
    /// Creates a clock starting at `initial_secs`.
    #[must_use] 
    pub fn new(initial_secs: u64) -> Self {
        Self {
            secs: Cell::new(initial_secs),
        }
    }

    /// Returns the current simulated time.
    pub fn now_secs(&self) -> u64 {
        self.secs.get()
    }

    /// Advances the clock by `delta` seconds.
    pub fn advance(&self, delta: u64) {
        self.secs.set(self.secs.get() + delta);
    }

    /// Sets the clock to an exact value (must be >= current).
    pub fn set(&self, secs: u64) {
        assert!(
            secs >= self.secs.get(),
            "SimClock::set: cannot go backwards ({} < {})",
            secs,
            self.secs.get()
        );
        self.secs.set(secs);
    }
}

impl Default for SimClock {
    fn default() -> Self {
        Self::new(1_700_000_000) // ~Nov 2023
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_clock() {
        let clock = SimClock::default();
        assert_eq!(clock.now_secs(), 1_700_000_000);
    }

    #[test]
    fn advance() {
        let clock = SimClock::new(100);
        clock.advance(10);
        assert_eq!(clock.now_secs(), 110);
        clock.advance(5);
        assert_eq!(clock.now_secs(), 115);
    }

    #[test]
    fn set_forward() {
        let clock = SimClock::new(100);
        clock.set(200);
        assert_eq!(clock.now_secs(), 200);
    }

    #[test]
    #[should_panic(expected = "cannot go backwards")]
    fn set_backward_panics() {
        let clock = SimClock::new(100);
        clock.set(50);
    }
}
