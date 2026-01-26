//! Resilient Exporter Wrappers
//!
//! This module provides decorator/wrapper exporters that add resilience patterns
//! on top of any base `SpanExporter` implementation:
//!
//! - [`RetryingExporter`]: Automatic retry with exponential backoff
//! - [`CircuitBreakerExporter`]: Fail-fast when backend is unhealthy
//! - [`RateLimitedExporter`]: Control export rate to avoid overwhelming backend
//!
//! # Rust 2024 Edition Features
//!
//! - Native async traits (no `#[async_trait]` macro)
//! - `impl Future<...> + Send` return types for async trait methods

use crate::exporter::{ExportError, SpanExporter};
use crate::rate_limiter::RateLimiter;
use crate::span::SpanBatch;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::{sleep, Instant};

// =============================================================================
// RETRY CONFIGURATION
// =============================================================================

/// Configuration for retry behavior.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts (0 = no retries, just the initial attempt).
    pub max_retries: u32,
    /// Initial delay before first retry.
    pub initial_delay: Duration,
    /// Maximum delay between retries (caps exponential growth).
    pub max_delay: Duration,
    /// Multiplier for exponential backoff (e.g., 2.0 = double delay each retry).
    pub backoff_multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(10),
            backoff_multiplier: 2.0,
        }
    }
}

impl RetryConfig {
    /// Calculate delay for a given attempt number (0-indexed).
    fn delay_for_attempt(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            return Duration::ZERO;
        }
        let delay_ms = self.initial_delay.as_millis() as f64
            * self.backoff_multiplier.powi((attempt - 1) as i32);
        let delay = Duration::from_millis(delay_ms as u64);
        delay.min(self.max_delay)
    }
}

// =============================================================================
// RETRYING EXPORTER
// =============================================================================

/// An exporter wrapper that automatically retries failed exports with exponential backoff.
///
/// # Example
///
/// ```ignore
/// let base_exporter = StdoutExporter::new(true);
/// let retrying = RetryingExporter::new(base_exporter, RetryConfig::default());
///
/// // Exports will be retried up to 3 times on failure
/// retrying.export(batch).await?;
/// ```
pub struct RetryingExporter<E: SpanExporter> {
    inner: E,
    config: RetryConfig,
    /// Metrics: total retry attempts made
    total_retries: AtomicU64,
    /// Metrics: successful exports after retry (not first attempt)
    recovered_exports: AtomicU64,
}

impl<E: SpanExporter> RetryingExporter<E> {
    /// Create a new retrying exporter with the given configuration.
    pub fn new(inner: E, config: RetryConfig) -> Self {
        Self {
            inner,
            config,
            total_retries: AtomicU64::new(0),
            recovered_exports: AtomicU64::new(0),
        }
    }

    /// Create with default retry configuration.
    pub fn with_defaults(inner: E) -> Self {
        Self::new(inner, RetryConfig::default())
    }

    /// Returns the total number of retry attempts made.
    pub fn total_retries(&self) -> u64 {
        self.total_retries.load(Ordering::Relaxed)
    }

    /// Returns exports that succeeded after at least one retry.
    pub fn recovered_exports(&self) -> u64 {
        self.recovered_exports.load(Ordering::Relaxed)
    }
}

impl<E: SpanExporter> SpanExporter for RetryingExporter<E> {
    async fn export(&self, batch: SpanBatch) -> Result<(), ExportError> {
        let max_attempts = self.config.max_retries + 1; // +1 for initial attempt

        for attempt in 0..max_attempts {
            // INV-RES-01: Verify attempt is bounded
            debug_assert!(
                attempt < max_attempts,
                "INV-RES-01: attempt {} exceeds max_attempts {}",
                attempt,
                max_attempts
            );

            // Wait before retry (no delay on first attempt)
            let delay = self.config.delay_for_attempt(attempt);
            if !delay.is_zero() {
                // INV-RES-02: Verify exponential backoff is capped
                debug_assert!(
                    delay <= self.config.max_delay,
                    "INV-RES-02: delay {:?} exceeds max_delay {:?}",
                    delay,
                    self.config.max_delay
                );
                self.total_retries.fetch_add(1, Ordering::Relaxed);
                sleep(delay).await;
            }

            // Clone batch for retry (SpanBatch must be Clone)
            let batch_clone = batch.clone();

            match self.inner.export(batch_clone).await {
                Ok(()) => {
                    if attempt > 0 {
                        self.recovered_exports.fetch_add(1, Ordering::Relaxed);
                    }
                    return Ok(());
                }
                Err(e) => {
                    // Don't retry circuit breaker open errors
                    if matches!(e, ExportError::CircuitOpen) {
                        return Err(e);
                    }
                    // Continue to next attempt
                }
            }
        }

        Err(ExportError::RetriesExhausted {
            attempts: max_attempts,
        })
    }

    fn name(&self) -> &str {
        // Could prefix with "retrying:" but keeping inner name for simplicity
        self.inner.name()
    }
}

// =============================================================================
// CIRCUIT BREAKER
// =============================================================================

/// Circuit breaker state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation - requests flow through.
    Closed,
    /// Backend unhealthy - requests fail fast.
    Open,
    /// Testing if backend recovered - allow one request through.
    HalfOpen,
}

/// Configuration for circuit breaker behavior.
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Number of consecutive failures before opening the circuit.
    pub failure_threshold: u32,
    /// How long to wait before transitioning from Open to HalfOpen.
    pub reset_timeout: Duration,
    /// Number of successes in HalfOpen state required to close the circuit.
    pub success_threshold: u32,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            reset_timeout: Duration::from_secs(30),
            success_threshold: 2,
        }
    }
}

/// Internal state for circuit breaker (needs interior mutability).
struct CircuitBreakerState {
    state: CircuitState,
    consecutive_failures: u32,
    consecutive_successes: u32,
    last_failure_time: Option<Instant>,
}

impl CircuitBreakerState {
    fn new() -> Self {
        Self {
            state: CircuitState::Closed,
            consecutive_failures: 0,
            consecutive_successes: 0,
            last_failure_time: None,
        }
    }
}

/// An exporter wrapper that implements the circuit breaker pattern.
///
/// When the backend fails repeatedly, the circuit "opens" and subsequent
/// requests fail immediately without attempting the export. After a cooldown
/// period, the circuit enters "half-open" state and allows a test request through.
///
/// # State Transitions
///
/// ```text
/// ┌────────┐  failure_threshold  ┌────────┐
/// │ Closed │ ──────────────────► │  Open  │
/// └────────┘                     └────────┘
///     ▲                              │
///     │ success_threshold            │ reset_timeout
///     │                              ▼
///     │                         ┌──────────┐
///     └──────────────────────── │ HalfOpen │
///           success             └──────────┘
///                                    │
///                                    │ failure
///                                    ▼
///                               ┌────────┐
///                               │  Open  │
///                               └────────┘
/// ```
pub struct CircuitBreakerExporter<E: SpanExporter> {
    inner: E,
    config: CircuitBreakerConfig,
    state: Mutex<CircuitBreakerState>,
    /// Metrics: times circuit has opened
    times_opened: AtomicU32,
}

impl<E: SpanExporter> CircuitBreakerExporter<E> {
    /// Create a new circuit breaker exporter with the given configuration.
    pub fn new(inner: E, config: CircuitBreakerConfig) -> Self {
        Self {
            inner,
            config,
            state: Mutex::new(CircuitBreakerState::new()),
            times_opened: AtomicU32::new(0),
        }
    }

    /// Create with default circuit breaker configuration.
    pub fn with_defaults(inner: E) -> Self {
        Self::new(inner, CircuitBreakerConfig::default())
    }

    /// Returns the current circuit state.
    pub fn state(&self) -> CircuitState {
        self.state.lock().unwrap().state
    }

    /// Returns how many times the circuit has opened.
    pub fn times_opened(&self) -> u32 {
        self.times_opened.load(Ordering::Relaxed)
    }

    /// Check if we should allow a request and potentially transition state.
    fn should_allow_request(&self) -> bool {
        let mut state = self.state.lock().unwrap();

        match state.state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                // Check if reset timeout has elapsed
                if let Some(last_failure) = state.last_failure_time
                    && last_failure.elapsed() >= self.config.reset_timeout {
                        state.state = CircuitState::HalfOpen;
                        state.consecutive_successes = 0;
                        return true;
                    }
                false
            }
            CircuitState::HalfOpen => true,
        }
    }

    /// Record a successful export.
    fn record_success(&self) {
        let mut state = self.state.lock().unwrap();
        let old_state = state.state;

        match state.state {
            CircuitState::Closed => {
                state.consecutive_failures = 0;
            }
            CircuitState::HalfOpen => {
                state.consecutive_successes += 1;
                if state.consecutive_successes >= self.config.success_threshold {
                    state.state = CircuitState::Closed;
                    state.consecutive_failures = 0;
                }
            }
            CircuitState::Open => {
                // Shouldn't happen, but reset to closed if it does
                state.state = CircuitState::Closed;
                state.consecutive_failures = 0;
            }
        }

        // INV-RES-03: Verify valid state transitions on success
        debug_assert!(
            matches!(
                (old_state, state.state),
                (CircuitState::Closed, CircuitState::Closed)
                    | (CircuitState::HalfOpen, CircuitState::HalfOpen)
                    | (CircuitState::HalfOpen, CircuitState::Closed)
                    | (CircuitState::Open, CircuitState::Closed) // recovery case
            ),
            "INV-RES-03: Invalid state transition on success: {:?} -> {:?}",
            old_state,
            state.state
        );
    }

    /// Record a failed export.
    fn record_failure(&self) {
        let mut state = self.state.lock().unwrap();
        let old_state = state.state;

        state.last_failure_time = Some(Instant::now());
        state.consecutive_successes = 0;

        match state.state {
            CircuitState::Closed => {
                state.consecutive_failures += 1;
                if state.consecutive_failures >= self.config.failure_threshold {
                    state.state = CircuitState::Open;
                    self.times_opened.fetch_add(1, Ordering::Relaxed);
                }
            }
            CircuitState::HalfOpen => {
                // Single failure in half-open reopens the circuit
                state.state = CircuitState::Open;
                self.times_opened.fetch_add(1, Ordering::Relaxed);
            }
            CircuitState::Open => {
                // Already open, just update failure time
            }
        }

        // INV-RES-03: Verify valid state transitions on failure
        debug_assert!(
            matches!(
                (old_state, state.state),
                (CircuitState::Closed, CircuitState::Closed)
                    | (CircuitState::Closed, CircuitState::Open)
                    | (CircuitState::HalfOpen, CircuitState::Open)
                    | (CircuitState::Open, CircuitState::Open)
            ),
            "INV-RES-03: Invalid state transition on failure: {:?} -> {:?}",
            old_state,
            state.state
        );
    }
}

impl<E: SpanExporter> SpanExporter for CircuitBreakerExporter<E> {
    async fn export(&self, batch: SpanBatch) -> Result<(), ExportError> {
        if !self.should_allow_request() {
            return Err(ExportError::CircuitOpen);
        }

        match self.inner.export(batch).await {
            Ok(()) => {
                self.record_success();
                Ok(())
            }
            Err(e) => {
                self.record_failure();
                Err(e)
            }
        }
    }

    fn name(&self) -> &str {
        self.inner.name()
    }
}

// =============================================================================
// RATE LIMITED EXPORTER
// =============================================================================

/// An exporter wrapper that rate-limits export operations.
///
/// Useful for preventing overwhelming the backend with too many requests.
///
/// # Example
///
/// ```ignore
/// let base = StdoutExporter::new(true);
/// let limiter = IntervalRateLimiter::from_rate(10.0); // 10 exports/sec
/// let rate_limited = RateLimitedExporter::new(base, limiter);
/// ```
pub struct RateLimitedExporter<E: SpanExporter, R: RateLimiter> {
    inner: E,
    rate_limiter: tokio::sync::Mutex<R>,
}

impl<E: SpanExporter, R: RateLimiter> RateLimitedExporter<E, R> {
    /// Create a new rate-limited exporter.
    pub fn new(inner: E, rate_limiter: R) -> Self {
        Self {
            inner,
            rate_limiter: tokio::sync::Mutex::new(rate_limiter),
        }
    }
}

impl<E: SpanExporter, R: RateLimiter + Send> SpanExporter for RateLimitedExporter<E, R> {
    async fn export(&self, batch: SpanBatch) -> Result<(), ExportError> {
        // Use tokio's async Mutex to avoid holding std::sync::Mutex across await
        {
            let mut limiter = self.rate_limiter.lock().await;
            limiter.wait().await;
        }

        self.inner.export(batch).await
    }

    fn name(&self) -> &str {
        self.inner.name()
    }
}

// =============================================================================
// COMPOSED RESILIENT EXPORTER
// =============================================================================

/// A fully resilient exporter with circuit breaker and retry.
///
/// Composition order (outer to inner):
/// 1. Circuit Breaker - fails fast when backend unhealthy
/// 2. Retry - retries transient failures
/// 3. Base Exporter - actual export logic
///
/// For rate limiting, use `RateLimitedExporter` separately.
///
/// # Example
///
/// ```ignore
/// // Wrap with retry and circuit breaker
/// let retrying = RetryingExporter::new(StdoutExporter::new(true), RetryConfig::default());
/// let resilient = CircuitBreakerExporter::new(retrying, CircuitBreakerConfig::default());
/// ```
pub struct ResilientExporterBuilder<E: SpanExporter> {
    inner: E,
    retry_config: Option<RetryConfig>,
    circuit_config: Option<CircuitBreakerConfig>,
}

impl<E: SpanExporter + 'static> ResilientExporterBuilder<E> {
    /// Start building a resilient exporter.
    pub fn new(inner: E) -> Self {
        Self {
            inner,
            retry_config: None,
            circuit_config: None,
        }
    }

    /// Add retry capability.
    pub fn with_retry(mut self, config: RetryConfig) -> Self {
        self.retry_config = Some(config);
        self
    }

    /// Add circuit breaker capability.
    pub fn with_circuit_breaker(mut self, config: CircuitBreakerConfig) -> Self {
        self.circuit_config = Some(config);
        self
    }

    /// Build the exporter with retry only.
    ///
    /// Returns the concrete type for zero-cost abstraction.
    pub fn build_retry_only(self) -> RetryingExporter<E> {
        RetryingExporter::new(self.inner, self.retry_config.unwrap_or_default())
    }

    /// Build the exporter with circuit breaker only.
    ///
    /// Returns the concrete type for zero-cost abstraction.
    pub fn build_circuit_breaker_only(self) -> CircuitBreakerExporter<E> {
        CircuitBreakerExporter::new(self.inner, self.circuit_config.unwrap_or_default())
    }

    /// Build with both retry (inner) and circuit breaker (outer).
    ///
    /// Returns the concrete type for zero-cost abstraction.
    pub fn build_with_retry_and_circuit_breaker(
        self,
    ) -> CircuitBreakerExporter<RetryingExporter<E>> {
        let retrying = RetryingExporter::new(self.inner, self.retry_config.unwrap_or_default());
        CircuitBreakerExporter::new(retrying, self.circuit_config.unwrap_or_default())
    }
}

// =============================================================================
// TESTS
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exporter::NullExporter;
    use crate::span::{Span, SpanKind};
    use std::sync::atomic::AtomicU32;

    /// An exporter that fails a configurable number of times before succeeding.
    struct FailingExporter {
        failures_remaining: AtomicU32,
        export_count: AtomicU32,
    }

    impl FailingExporter {
        fn new(fail_count: u32) -> Self {
            Self {
                failures_remaining: AtomicU32::new(fail_count),
                export_count: AtomicU32::new(0),
            }
        }

        #[allow(dead_code)]
        fn export_count(&self) -> u32 {
            self.export_count.load(Ordering::Relaxed)
        }
    }

    impl SpanExporter for FailingExporter {
        async fn export(&self, _batch: SpanBatch) -> Result<(), ExportError> {
            self.export_count.fetch_add(1, Ordering::Relaxed);

            let remaining = self.failures_remaining.fetch_sub(1, Ordering::Relaxed);
            if remaining > 0 {
                Err(ExportError::Transport("simulated failure".into()))
            } else {
                Ok(())
            }
        }

        fn name(&self) -> &str {
            "failing"
        }
    }

    fn make_test_batch() -> SpanBatch {
        let mut batch = SpanBatch::new();
        batch.add(Span::new(1, 1, 0, "test".into(), SpanKind::Internal));
        batch
    }

    #[tokio::test]
    async fn test_retry_succeeds_after_failures() {
        let base = FailingExporter::new(2); // Fail twice, then succeed
        let retrying = RetryingExporter::new(
            base,
            RetryConfig {
                max_retries: 3,
                initial_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(10),
                backoff_multiplier: 2.0,
            },
        );

        let result = retrying.export(make_test_batch()).await;
        assert!(result.is_ok());
        assert_eq!(retrying.total_retries(), 2);
        assert_eq!(retrying.recovered_exports(), 1);
    }

    #[tokio::test]
    async fn test_retry_exhausted() {
        let base = FailingExporter::new(10); // Always fail
        let retrying = RetryingExporter::new(
            base,
            RetryConfig {
                max_retries: 2,
                initial_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(10),
                backoff_multiplier: 2.0,
            },
        );

        let result = retrying.export(make_test_batch()).await;
        assert!(matches!(
            result,
            Err(ExportError::RetriesExhausted { attempts: 3 })
        ));
    }

    #[tokio::test]
    async fn test_circuit_breaker_opens_on_failures() {
        let base = FailingExporter::new(100); // Always fail
        let cb = CircuitBreakerExporter::new(
            base,
            CircuitBreakerConfig {
                failure_threshold: 3,
                reset_timeout: Duration::from_secs(60),
                success_threshold: 1,
            },
        );

        // First 3 failures should go through
        for _ in 0..3 {
            let _ = cb.export(make_test_batch()).await;
        }

        assert_eq!(cb.state(), CircuitState::Open);
        assert_eq!(cb.times_opened(), 1);

        // Next request should fail fast
        let result = cb.export(make_test_batch()).await;
        assert!(matches!(result, Err(ExportError::CircuitOpen)));
    }

    #[tokio::test]
    async fn test_circuit_breaker_half_open_recovery() {
        // FailingExporter that fails 3 times then succeeds
        // Use a fresh exporter wrapped directly (no Arc needed)
        let base = FailingExporter::new(3);
        let cb = CircuitBreakerExporter::new(
            base,
            CircuitBreakerConfig {
                failure_threshold: 3,
                reset_timeout: Duration::from_millis(10),
                success_threshold: 1,
            },
        );

        // Trigger circuit open
        for _ in 0..3 {
            let _ = cb.export(make_test_batch()).await;
        }
        assert_eq!(cb.state(), CircuitState::Open);

        // Wait for reset timeout
        sleep(Duration::from_millis(20)).await;

        // Next request should be allowed (half-open) and succeed
        let result = cb.export(make_test_batch()).await;
        assert!(result.is_ok());
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[tokio::test]
    async fn test_rate_limited_exporter() {
        use crate::rate_limiter::IntervalRateLimiter;

        let base = NullExporter::new();
        let limiter = IntervalRateLimiter::from_rate(100.0); // 100/sec = 10ms interval
        let rate_limited = RateLimitedExporter::new(base, limiter);

        let start = std::time::Instant::now();
        for _ in 0..5 {
            rate_limited.export(make_test_batch()).await.unwrap();
        }
        let elapsed = start.elapsed();

        // 5 exports at 10ms each = ~40-50ms (first is immediate)
        assert!(
            elapsed >= Duration::from_millis(30),
            "Expected >= 30ms, got {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn test_builder_composition() {
        // Build with both retry and circuit breaker using concrete types
        let exporter = ResilientExporterBuilder::new(NullExporter::new())
            .with_retry(RetryConfig::default())
            .with_circuit_breaker(CircuitBreakerConfig::default())
            .build_with_retry_and_circuit_breaker();

        // Should work without errors
        let result = exporter.export(make_test_batch()).await;
        assert!(result.is_ok());
    }
}
