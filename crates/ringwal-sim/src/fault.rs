//! Fault injection configuration for deterministic simulation.
//!
//! Controls the probability of various failure modes. A seeded RNG
//! consults these rates to decide whether each operation should fail.

/// Failure probabilities for simulated I/O operations.
///
/// All rates are in the range `0.0..=1.0`. A rate of `0.0` means the
/// fault never fires; `1.0` means every operation fails.
///
/// # Example
///
/// ```
/// use ringwal_sim::FaultConfig;
///
/// let config = FaultConfig::builder()
///     .write_fail_rate(0.01)
///     .fsync_fail_rate(0.05)
///     .crash_probability(0.001)
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct FaultConfig {
    /// Probability that a `write()` / `write_all()` call returns an I/O error.
    pub write_fail_rate: f64,
    /// Probability that `sync_all()` / `sync_data()` returns an I/O error.
    /// Data that was flushed to the kernel buffer is *not* promoted to durable.
    pub fsync_fail_rate: f64,
    /// Probability that a write is truncated to a random shorter length.
    /// Simulates torn writes (OS crash mid-write).
    pub partial_write_rate: f64,
    /// Per-operation probability that a simulated crash occurs *after* the
    /// operation completes. Discards `write_buffer` + `kernel_buffer`, keeps
    /// only durable data.
    pub crash_probability: f64,
    /// Probability that `open_read()` returns `NotFound` even if the file
    /// exists. Simulates transient filesystem errors.
    pub read_fail_rate: f64,
}

impl FaultConfig {
    /// No faults — all operations succeed. Useful for baseline correctness tests.
    #[must_use] 
    pub fn none() -> Self {
        Self {
            write_fail_rate: 0.0,
            fsync_fail_rate: 0.0,
            partial_write_rate: 0.0,
            crash_probability: 0.0,
            read_fail_rate: 0.0,
        }
    }

    /// Returns a builder for constructing a `FaultConfig`.
    #[must_use] 
    pub fn builder() -> FaultConfigBuilder {
        FaultConfigBuilder::default()
    }
}

impl Default for FaultConfig {
    fn default() -> Self {
        Self::none()
    }
}

/// Builder for [`FaultConfig`].
#[derive(Debug, Clone, Default)]
pub struct FaultConfigBuilder {
    write_fail_rate: f64,
    fsync_fail_rate: f64,
    partial_write_rate: f64,
    crash_probability: f64,
    read_fail_rate: f64,
}

impl FaultConfigBuilder {
    #[must_use] 
    pub fn write_fail_rate(mut self, rate: f64) -> Self {
        assert!((0.0..=1.0).contains(&rate), "rate must be 0.0..=1.0");
        self.write_fail_rate = rate;
        self
    }

    #[must_use] 
    pub fn fsync_fail_rate(mut self, rate: f64) -> Self {
        assert!((0.0..=1.0).contains(&rate), "rate must be 0.0..=1.0");
        self.fsync_fail_rate = rate;
        self
    }

    #[must_use] 
    pub fn partial_write_rate(mut self, rate: f64) -> Self {
        assert!((0.0..=1.0).contains(&rate), "rate must be 0.0..=1.0");
        self.partial_write_rate = rate;
        self
    }

    #[must_use] 
    pub fn crash_probability(mut self, rate: f64) -> Self {
        assert!((0.0..=1.0).contains(&rate), "rate must be 0.0..=1.0");
        self.crash_probability = rate;
        self
    }

    #[must_use] 
    pub fn read_fail_rate(mut self, rate: f64) -> Self {
        assert!((0.0..=1.0).contains(&rate), "rate must be 0.0..=1.0");
        self.read_fail_rate = rate;
        self
    }

    #[must_use] 
    pub fn build(self) -> FaultConfig {
        FaultConfig {
            write_fail_rate: self.write_fail_rate,
            fsync_fail_rate: self.fsync_fail_rate,
            partial_write_rate: self.partial_write_rate,
            crash_probability: self.crash_probability,
            read_fail_rate: self.read_fail_rate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_is_all_zero() {
        let fc = FaultConfig::none();
        assert_eq!(fc.write_fail_rate, 0.0);
        assert_eq!(fc.fsync_fail_rate, 0.0);
        assert_eq!(fc.partial_write_rate, 0.0);
        assert_eq!(fc.crash_probability, 0.0);
        assert_eq!(fc.read_fail_rate, 0.0);
    }

    #[test]
    fn builder_sets_rates() {
        let fc = FaultConfig::builder()
            .write_fail_rate(0.1)
            .fsync_fail_rate(0.2)
            .partial_write_rate(0.05)
            .crash_probability(0.01)
            .read_fail_rate(0.03)
            .build();
        assert_eq!(fc.write_fail_rate, 0.1);
        assert_eq!(fc.fsync_fail_rate, 0.2);
        assert_eq!(fc.partial_write_rate, 0.05);
        assert_eq!(fc.crash_probability, 0.01);
        assert_eq!(fc.read_fail_rate, 0.03);
    }

    #[test]
    #[should_panic(expected = "rate must be 0.0..=1.0")]
    fn rate_out_of_range() {
        let _ = FaultConfig::builder().write_fail_rate(1.5);
    }
}
