use std::hint;
use std::thread;

/// Adaptive backoff strategy (Crossbeam-style).
///
/// Progressively increases wait time: spin with PAUSE → yield to OS → give up.
#[derive(Debug)]
pub struct Backoff {
    step: u32,
}

impl Backoff {
    const SPIN_LIMIT: u32 = 6; // 2^6 = 64 spins max before yielding
    const YIELD_LIMIT: u32 = 10; // Then give up

    /// Creates a new backoff instance.
    #[inline]
    pub fn new() -> Self {
        Self { step: 0 }
    }

    /// Light spin with PAUSE hints.
    #[inline]
    pub fn spin(&mut self) {
        let spins = 1 << self.step.min(Self::SPIN_LIMIT);
        for _ in 0..spins {
            hint::spin_loop();
        }
        if self.step <= Self::SPIN_LIMIT {
            self.step += 1;
        }
    }

    /// Heavier backoff: spin then yield.
    #[inline]
    pub fn snooze(&mut self) {
        if self.step <= Self::SPIN_LIMIT {
            self.spin();
        } else {
            thread::yield_now();
            if self.step <= Self::YIELD_LIMIT {
                self.step += 1;
            }
        }
    }

    /// Check if we've exhausted patience.
    #[inline]
    pub fn is_completed(&self) -> bool {
        self.step > Self::YIELD_LIMIT
    }

    /// Reset for next wait cycle.
    #[inline]
    pub fn reset(&mut self) {
        self.step = 0;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backoff_progression() {
        let mut b = Backoff::new();

        // Should start at step 0
        assert_eq!(b.step, 0);

        // Spin should increment
        b.spin();
        assert!(b.step > 0);

        // Should eventually complete
        while !b.is_completed() {
            b.snooze();
        }
        assert!(b.step > Backoff::YIELD_LIMIT);

        // Reset
        b.reset();
        assert_eq!(b.step, 0);
    }
}
