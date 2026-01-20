use crate::span::Span;
use ringmpsc_rs::{Channel, ChannelError, Config, Producer};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;

/// Configuration for the span collector
#[derive(Debug, Clone)]
pub struct CollectorConfig {
    /// Ring buffer size as power of 2 (1-20)
    pub ring_bits: u8,
    /// Maximum number of producers
    pub max_producers: u8,
    /// Enable performance metrics
    pub enable_metrics: bool,
}

impl Default for CollectorConfig {
    fn default() -> Self {
        // Use LOW_LATENCY_CONFIG equivalent
        Self {
            ring_bits: 12,       // 4K spans per ring
            max_producers: 16,   // Typical async runtime has 4-8 threads
            enable_metrics: true,
        }
    }
}

impl From<CollectorConfig> for Config {
    fn from(config: CollectorConfig) -> Self {
        Config {
            ring_bits: config.ring_bits,
            max_producers: config.max_producers as usize,
            enable_metrics: config.enable_metrics,
        }
    }
}

/// Metrics for the span collector
#[derive(Debug, Default)]
pub struct CollectorMetrics {
    /// Total spans submitted
    pub spans_submitted: AtomicU64,
    /// Total spans consumed
    pub spans_consumed: AtomicU64,
    /// Number of times ring was full
    pub full_events: AtomicU64,
    /// Number of reserve retries
    pub reserve_retries: AtomicU64,
}

// All methods use `Ordering::Relaxed` because these are purely statistical counters:
//
// 1. No control flow dependencies - No code path depends on these values being "up to date"
// 2. Eventual visibility is acceptable - Slightly stale reads are fine for observability
// 3. No happens-before relationships needed - Unlike ring buffer head/tail, these don't
//    guard any other data or coordinate producer-consumer handoff
// 4. Maximum performance - Relaxed avoids memory barriers in hot paths (submit loops)
//
// This is the standard pattern for metrics/counters in high-performance code.
impl CollectorMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn spans_submitted(&self) -> u64 {
        self.spans_submitted.load(Ordering::Relaxed)
    }

    pub fn spans_consumed(&self) -> u64 {
        self.spans_consumed.load(Ordering::Relaxed)
    }

    pub fn full_events(&self) -> u64 {
        self.full_events.load(Ordering::Relaxed)
    }

    pub fn reserve_retries(&self) -> u64 {
        self.reserve_retries.load(Ordering::Relaxed)
    }
}

/// Error types for span submission
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SubmitError {
    /// Ring buffer is full
    #[error("ring buffer is full")]
    Full,
    /// Channel is closed
    #[error("channel is closed")]
    Closed,
}

/// Core synchronous span collector using lock-free MPSC channels
pub struct SpanCollector {
    channel: Arc<Channel<Span>>,
    config: CollectorConfig,
    metrics: Arc<CollectorMetrics>,
}

impl SpanCollector {
    /// Creates a new span collector with the given configuration
    pub fn new(config: CollectorConfig) -> Self {
        let channel = Channel::new(config.clone().into());
        Self {
            channel: Arc::new(channel),
            config,
            metrics: Arc::new(CollectorMetrics::new()),
        }
    }

    /// Registers a new span producer
    pub fn register(&self) -> Result<SpanProducer, ChannelError> {
        let producer = self.channel.register()?;
        Ok(SpanProducer {
            producer,
            metrics: Arc::clone(&self.metrics),
        })
    }

    /// Returns the underlying channel
    pub fn channel(&self) -> &Arc<Channel<Span>> {
        &self.channel
    }

    /// Returns the collector configuration
    pub fn config(&self) -> &CollectorConfig {
        &self.config
    }

    /// Returns collector metrics
    pub fn metrics(&self) -> &Arc<CollectorMetrics> {
        &self.metrics
    }

    /// Consumes up to `limit` spans from all producers, transferring ownership.
    ///
    /// # Design Note
    ///
    /// This collector only provides owned consumption APIs because `Span` contains
    /// heap-allocated fields (`String`, `Box<HashMap>`) making cloning expensive.
    /// Reference-based consumption would force callers to clone, which defeats the
    /// purpose of zero-copy ring buffer design.
    ///
    /// For generic use cases where `T` might be `Copy`, see the core library's
    /// `Channel::consume_all_up_to` which provides both reference and owned variants.
    pub fn consume_all_up_to<F>(&self, limit: usize, mut f: F) -> usize
    where
        F: FnMut(Span),
    {
        let mut consumed = 0;
        let result = self.channel.consume_all_up_to_owned(limit, |span| {
            f(span);
            consumed += 1;
        });
        
        if consumed > 0 {
            self.metrics
                .spans_consumed
                .fetch_add(consumed as u64, Ordering::Relaxed);
        }
        
        result
    }

    /// Consumes all available spans from all producers, transferring ownership.
    ///
    /// # Design Note
    ///
    /// This collector only provides owned consumption APIs because `Span` contains
    /// heap-allocated fields (`String`, `Box<HashMap>`) making cloning expensive.
    /// See [`consume_all_up_to`] for detailed rationale.
    pub fn consume_all<F>(&self, mut f: F) -> usize
    where
        F: FnMut(Span),
    {
        let mut consumed = 0;
        let result = self.channel.consume_all_owned(|span| {
            f(span);
            consumed += 1;
        });
        
        if consumed > 0 {
            self.metrics
                .spans_consumed
                .fetch_add(consumed as u64, Ordering::Relaxed);
        }
        
        result
    }

    /// Closes the channel, preventing new producers from registering
    pub fn close(&self) {
        self.channel.close();
    }
}

/// Handle for submitting spans to the collector
pub struct SpanProducer {
    producer: Producer<Span>,
    metrics: Arc<CollectorMetrics>,
}

impl SpanProducer {
    /// Submits a span with retry and backoff
    pub fn submit_span(&self, span: Span) -> Result<(), SubmitError> {
        // Try to reserve with backoff
        match self.producer.reserve_with_backoff(1) {
            Some(mut reservation) => {
                // Write span to reservation
                let slice = reservation.as_mut_slice();
                slice[0].write(span);
                reservation.commit();
                
                self.metrics
                    .spans_submitted
                    .fetch_add(1, Ordering::Relaxed);
                
                Ok(())
            }
            None => {
                self.metrics.full_events.fetch_add(1, Ordering::Relaxed);
                Err(SubmitError::Full)
            }
        }
    }

    /// Tries to submit a span without blocking
    pub fn try_submit_span(&self, span: Span) -> Result<(), SubmitError> {
        match self.producer.reserve(1) {
            Some(mut reservation) => {
                let slice = reservation.as_mut_slice();
                slice[0].write(span);
                reservation.commit();
                
                self.metrics
                    .spans_submitted
                    .fetch_add(1, Ordering::Relaxed);
                
                Ok(())
            }
            None => {
                self.metrics.full_events.fetch_add(1, Ordering::Relaxed);
                Err(SubmitError::Full)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::SpanKind;

    #[test]
    fn test_submit_and_consume_span() {
        let collector = SpanCollector::new(CollectorConfig::default());
        let producer = collector.register().unwrap();

        let span = Span::new(12345, 1, 0, "test-op".to_string(), SpanKind::Internal);
        producer.submit_span(span).unwrap();

        let mut consumed = Vec::new();
        collector.consume_all(|span| {
            consumed.push(span);  // Zero-copy: ownership transferred
        });

        assert_eq!(consumed.len(), 1);
        assert_eq!(consumed[0].trace_id, 12345);
        assert_eq!(consumed[0].name, "test-op");
    }

    #[test]
    fn test_multiple_producers() {
        let collector = SpanCollector::new(CollectorConfig::default());
        
        let producer1 = collector.register().unwrap();
        let producer2 = collector.register().unwrap();

        let span1 = Span::new(1, 1, 0, "op1".to_string(), SpanKind::Server);
        let span2 = Span::new(2, 2, 0, "op2".to_string(), SpanKind::Client);

        producer1.submit_span(span1).unwrap();
        producer2.submit_span(span2).unwrap();

        let mut consumed = Vec::new();
        collector.consume_all(|span| {
            consumed.push(span);  // Zero-copy: ownership transferred
        });

        assert_eq!(consumed.len(), 2);
    }

    #[test]
    fn test_metrics() {
        let collector = SpanCollector::new(CollectorConfig::default());
        let producer = collector.register().unwrap();

        for i in 0..10 {
            let span = Span::new(i as u128, i, 0, format!("op-{}", i), SpanKind::Internal);
            producer.submit_span(span).unwrap();
        }

        assert_eq!(collector.metrics().spans_submitted(), 10);

        collector.consume_all(|_span| {
            // Span is dropped here after processing
        });

        assert_eq!(collector.metrics().spans_consumed(), 10);
    }
}
