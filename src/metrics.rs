use std::sync::atomic::{AtomicU64, Ordering};

/// Thread-safe metrics for monitoring channel performance.
/// 
/// Uses atomic counters with `Relaxed` ordering since these are purely
/// statistical - no control flow depends on exact values, and eventual
/// visibility is acceptable for observability.
#[derive(Debug)]
pub struct Metrics {
    messages_sent: AtomicU64,
    messages_received: AtomicU64,
    batches_sent: AtomicU64,
    batches_received: AtomicU64,
    reserve_spins: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            messages_sent: AtomicU64::new(0),
            messages_received: AtomicU64::new(0),
            batches_sent: AtomicU64::new(0),
            batches_received: AtomicU64::new(0),
            reserve_spins: AtomicU64::new(0),
        }
    }

    /// Increment messages sent counter.
    #[inline]
    pub fn add_messages_sent(&self, n: u64) {
        self.messages_sent.fetch_add(n, Ordering::Relaxed);
    }

    /// Increment batches sent counter.
    #[inline]
    pub fn add_batches_sent(&self, n: u64) {
        self.batches_sent.fetch_add(n, Ordering::Relaxed);
    }

    /// Increment messages received counter.
    #[inline]
    pub fn add_messages_received(&self, n: u64) {
        self.messages_received.fetch_add(n, Ordering::Relaxed);
    }

    /// Increment batches received counter.
    #[inline]
    pub fn add_batches_received(&self, n: u64) {
        self.batches_received.fetch_add(n, Ordering::Relaxed);
    }

    /// Increment reserve spins counter.
    #[inline]
    pub fn add_reserve_spins(&self, n: u64) {
        self.reserve_spins.fetch_add(n, Ordering::Relaxed);
    }

    /// Take a snapshot of current metrics values.
    /// 
    /// Returns a plain struct with `u64` values that can be copied and compared.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            messages_sent: self.messages_sent.load(Ordering::Relaxed),
            messages_received: self.messages_received.load(Ordering::Relaxed),
            batches_sent: self.batches_sent.load(Ordering::Relaxed),
            batches_received: self.batches_received.load(Ordering::Relaxed),
            reserve_spins: self.reserve_spins.load(Ordering::Relaxed),
        }
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// A point-in-time snapshot of metrics values.
/// 
/// This is a plain data struct (Copy, Clone) for easy use in aggregation and display.
#[derive(Debug, Clone, Copy, Default)]
pub struct MetricsSnapshot {
    pub messages_sent: u64,
    pub messages_received: u64,
    pub batches_sent: u64,
    pub batches_received: u64,
    pub reserve_spins: u64,
}
