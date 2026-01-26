//! Rate Limiting Abstractions
//!
//! Provides a trait-based rate limiting abstraction for controlling span generation
//! rates in producer tasks. The design is decoupled to allow easy swapping of
//! implementations (interval-based, token bucket, adaptive, etc.).
//!
//! # Rust 2024 Edition
//!
//! This module uses native async traits (no `#[async_trait]` macro required).

use std::time::Duration;
use tokio::time::{interval, Interval, MissedTickBehavior};

/// Trait for rate limiting async operations.
///
/// Implementors control the pacing of operations by awaiting `wait()` between
/// each operation. Uses native async fn in traits (Rust 2024 edition).
///
/// # Example
///
/// ```ignore
/// let mut limiter = IntervalRateLimiter::from_rate(100.0); // 100 ops/sec
/// loop {
///     limiter.wait().await;
///     do_work();
/// }
/// ```
///
/// # Note on Send bounds
///
/// The `+ Send` bound on the return type ensures the future can be used in
/// multi-threaded async runtimes like Tokio. This is required because `async fn`
/// in traits doesn't automatically add `Send` bounds.
pub trait RateLimiter: Send {
    /// Wait until the next operation is permitted.
    ///
    /// This method should be called before each rate-limited operation.
    /// It will complete immediately if within budget, or delay as needed.
    fn wait(&mut self) -> impl Future<Output = ()> + Send;

    /// Returns the target rate in operations per second, if known.
    fn target_rate(&self) -> Option<f64> {
        None
    }
}

use std::future::Future;
use std::pin::Pin;

/// Object-safe version of RateLimiter for dynamic dispatch.
///
/// This trait uses `Pin<Box<dyn Future>>` to allow `dyn RateLimiterBoxed`.
pub trait RateLimiterBoxed: Send {
    /// Wait until the next operation is permitted (boxed future for object safety).
    fn wait_boxed(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

    /// Returns the target rate in operations per second, if known.
    fn target_rate(&self) -> Option<f64> {
        None
    }
}

/// Blanket implementation: any RateLimiter can be used as RateLimiterBoxed
impl<T: RateLimiter> RateLimiterBoxed for T {
    fn wait_boxed(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(self.wait())
    }

    fn target_rate(&self) -> Option<f64> {
        RateLimiter::target_rate(self)
    }
}

/// Interval-based rate limiter using `tokio::time::Interval`.
///
/// Provides consistent pacing by waiting a fixed duration between operations.
/// Missed ticks are skipped (burst mode) to avoid queueing backpressure.
///
/// # Example
///
/// ```ignore
/// // 100 spans per second = 10ms interval
/// let limiter = IntervalRateLimiter::from_rate(100.0);
///
/// // Or specify interval directly
/// let limiter = IntervalRateLimiter::new(Duration::from_millis(10));
/// ```
pub struct IntervalRateLimiter {
    interval: Option<Interval>,
    rate_per_sec: f64,
}

impl IntervalRateLimiter {
    /// Create a rate limiter with a specific interval between operations.
    ///
    /// If `period` is zero, creates an unlimited rate limiter that only yields.
    pub fn new(period: Duration) -> Self {
        if period.is_zero() {
            return Self {
                interval: None,
                rate_per_sec: f64::INFINITY,
            };
        }

        let mut interval = interval(period);
        // Skip missed ticks to avoid queueing - if we fall behind, catch up by skipping
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let rate_per_sec = 1.0 / period.as_secs_f64();

        Self {
            interval: Some(interval),
            rate_per_sec,
        }
    }

    /// Create a rate limiter from a target rate in operations per second.
    ///
    /// # Panics
    ///
    /// Panics if `rate_per_sec` is zero or negative.
    pub fn from_rate(rate_per_sec: f64) -> Self {
        assert!(rate_per_sec > 0.0, "rate must be positive");
        let period = Duration::from_secs_f64(1.0 / rate_per_sec);
        let mut limiter = Self::new(period);
        limiter.rate_per_sec = rate_per_sec;
        limiter
    }

    /// Create an unlimited rate limiter that never waits.
    ///
    /// Useful for testing or when backpressure from the ring buffer
    /// is the only desired rate control.
    pub fn unlimited() -> Self {
        Self::new(Duration::ZERO)
    }
}

impl RateLimiter for IntervalRateLimiter {
    async fn wait(&mut self) {
        match &mut self.interval {
            Some(interval) => {
                interval.tick().await;
            }
            None => {
                // Unlimited: just yield to runtime
                tokio::task::yield_now().await;
            }
        }
    }

    fn target_rate(&self) -> Option<f64> {
        if self.rate_per_sec.is_infinite() {
            None
        } else {
            Some(self.rate_per_sec)
        }
    }
}

/// A no-op rate limiter that yields to the async runtime without waiting.
///
/// Useful when you want the ring buffer's backpressure (via `Notify`) to be
/// the sole rate control mechanism, while still yielding to prevent starving
/// other tasks.
pub struct YieldingRateLimiter;

impl RateLimiter for YieldingRateLimiter {
    async fn wait(&mut self) {
        tokio::task::yield_now().await;
    }

    fn target_rate(&self) -> Option<f64> {
        None // Unlimited - controlled by backpressure only
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn test_interval_rate_limiter_timing() {
        let mut limiter = IntervalRateLimiter::from_rate(100.0); // 100/sec = 10ms interval

        let start = Instant::now();
        for _ in 0..10 {
            limiter.wait().await;
        }
        let elapsed = start.elapsed();

        // 10 ticks at 10ms each = ~100ms (first tick is immediate)
        // Allow some tolerance for scheduling jitter
        assert!(
            elapsed >= Duration::from_millis(80) && elapsed <= Duration::from_millis(150),
            "Expected ~90ms, got {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn test_unlimited_rate_limiter() {
        let mut limiter = IntervalRateLimiter::unlimited();

        let start = Instant::now();
        for _ in 0..1000 {
            limiter.wait().await;
        }
        let elapsed = start.elapsed();

        // Should be very fast - just yielding
        assert!(
            elapsed < Duration::from_millis(50),
            "Unlimited limiter too slow: {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn test_target_rate() {
        let limiter = IntervalRateLimiter::from_rate(250.0);
        // Disambiguate between RateLimiter and RateLimiterBoxed traits
        assert_eq!(RateLimiter::target_rate(&limiter), Some(250.0));

        let unlimited = IntervalRateLimiter::unlimited();
        assert_eq!(RateLimiter::target_rate(&unlimited), None);
    }
}
