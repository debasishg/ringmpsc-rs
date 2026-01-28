//! Configuration for stream behavior.

use std::time::Duration;

/// Configuration for async stream behavior.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// Poll interval for hybrid polling strategy.
    /// 
    /// Even with event-driven notify, this interval acts as a safety net
    /// to catch missed notifications and batch small bursts.
    /// 
    /// Default: 10ms
    pub poll_interval: Duration,

    /// Target batch size hint for consumption.
    /// 
    /// The receiver will attempt to yield up to this many items per poll
    /// to improve throughput via batching.
    /// 
    /// Default: 64
    pub batch_hint: usize,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(10),
            batch_hint: 64,
        }
    }
}

impl StreamConfig {
    /// Creates a low-latency configuration with shorter poll interval.
    pub fn low_latency() -> Self {
        Self {
            poll_interval: Duration::from_millis(1),
            batch_hint: 16,
        }
    }

    /// Creates a high-throughput configuration with larger batches.
    pub fn high_throughput() -> Self {
        Self {
            poll_interval: Duration::from_millis(50),
            batch_hint: 256,
        }
    }

    /// Sets the poll interval.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Sets the batch hint.
    pub fn with_batch_hint(mut self, hint: usize) -> Self {
        self.batch_hint = hint;
        self
    }
}
