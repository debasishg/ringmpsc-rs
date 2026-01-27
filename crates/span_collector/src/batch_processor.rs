//! Batch Processor - Pure Batching Abstraction
//!
//! This module provides a **pure batching abstraction** with no concurrency overhead.
//! It groups spans by trace_id and decides when to flush based on size/time thresholds.
//!
//! # Design Philosophy: Separation of Concerns
//!
//! Concurrency is an *orthogonal concern* to batching. This module deliberately avoids:
//! - `Arc` wrappers (no shared ownership assumptions)
//! - `AtomicU64` counters (no memory barriers or cache invalidation)
//! - Storing the exporter (passed as parameter to `flush()`)
//!
//! This separation provides:
//!
//! 1. **Zero atomic overhead for sequential use** - `BatchMetrics` uses plain `u64`
//! 2. **Better compiler optimizations** - no memory barriers block auto-vectorization
//! 3. **Testable in isolation** - no async/concurrency machinery required
//! 4. **Explicit concurrency boundary** - concurrent code lives in `async_bridge`
//!
//! # Usage Patterns
//!
//! ## Sequential Export (Simple, Lower Overhead)
//!
//! For applications where export latency isn't a bottleneck:
//!
//! ```rust,ignore
//! use span_collector::{BatchProcessor, BatchConfig};
//!
//! let mut processor = BatchProcessor::new(BatchConfig::default());
//! let exporter = Arc::new(MyExporter::new());
//!
//! // Add spans
//! processor.add(span);
//!
//! // Check and flush sequentially
//! if processor.should_flush() {
//!     processor.flush(exporter.as_ref()).await?;
//! }
//!
//! // Access metrics (plain u64, no atomic loads)
//! println!("Exported: {}", processor.metrics().spans_exported);
//! ```
//!
//! ## Concurrent Export (Higher Throughput)
//!
//! For high-volume scenarios where export latency would block collection,
//! use `take_batch()` and manage concurrency externally:
//!
//! ```rust,ignore
//! use span_collector::{BatchProcessor, BatchConfig, ExportMetrics};
//! use tokio::sync::Semaphore;
//! use tokio::task::JoinSet;
//!
//! let mut processor = BatchProcessor::new(BatchConfig::default());
//! let exporter: Arc<dyn SpanExporterBoxed> = Arc::new(MyExporter::new());
//! let export_metrics = Arc::new(ExportMetrics::default());  // AtomicU64 for concurrency
//! let semaphore = Arc::new(Semaphore::new(4));  // Limit concurrent exports
//! let mut tasks: JoinSet<_> = JoinSet::new();
//!
//! // Add spans
//! processor.add(span);
//!
//! // Spawn concurrent export
//! if processor.should_flush() {
//!     if let Some(batch) = processor.take_batch() {
//!         let permit = semaphore.clone().acquire_owned().await?;
//!         let exporter = Arc::clone(&exporter);
//!         let metrics = Arc::clone(&export_metrics);
//!         let span_count = batch.spans.len() as u64;
//!
//!         tasks.spawn(async move {
//!             let result = exporter.export_boxed(batch).await;
//!             if result.is_ok() {
//!                 metrics.record_success(span_count);
//!             }
//!             drop(permit);
//!             result
//!         });
//!     }
//! }
//! ```
//!
//! # Metrics: BatchMetrics vs ExportMetrics
//!
//! | Type | Location | Fields | Use Case |
//! |------|----------|--------|----------|
//! | `BatchMetrics` | `batch_processor.rs` | Plain `u64` | Sequential export |
//! | `ExportMetrics` | `async_bridge.rs` | `AtomicU64` | Concurrent export tasks |
//!
//! The `AsyncSpanCollector` in `async_bridge` uses `ExportMetrics` internally
//! and handles all concurrency concerns (Semaphore, JoinSet, atomic metrics).
//! Most users should use `AsyncSpanCollector` rather than managing concurrency manually.
//!
//! # See Also
//!
//! - [`AsyncSpanCollector`](crate::AsyncSpanCollector) - High-level async collector with built-in concurrency
//! - [`ExportMetrics`](crate::ExportMetrics) - Thread-safe metrics for concurrent exports

use crate::exporter::{ExportError, SpanExporterBoxed};
use crate::span::{Span, SpanBatch};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;

/// Configuration for batch processing
#[derive(Debug, Clone)]
pub struct BatchConfig {
    /// Maximum number of spans per batch
    pub batch_size_limit: usize,
    /// Maximum time to wait before flushing a batch
    pub batch_timeout: Duration,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            batch_size_limit: 10_000,
            batch_timeout: Duration::from_secs(5),
        }
    }
}

/// Metrics for batch processing (plain u64 - no atomic overhead for sequential use)
#[derive(Debug, Default, Clone)]
pub struct BatchMetrics {
    /// Total spans exported
    pub spans_exported: u64,
    /// Total batches exported
    pub batches_exported: u64,
    /// Total export errors
    pub export_errors: u64,
}

impl BatchMetrics {
    /// Record a successful export
    pub fn record_success(&mut self, span_count: u64) {
        self.spans_exported += span_count;
        self.batches_exported += 1;
    }

    /// Record an export error
    pub fn record_error(&mut self) {
        self.export_errors += 1;
    }
}

/// Batch processor that groups spans by trace_id and decides when to flush.
///
/// This is a pure batching abstraction with no concurrency concerns.
/// For concurrent exports, use `take_batch()` and manage export tasks externally.
pub struct BatchProcessor {
    /// Pending spans grouped by trace_id
    pending: HashMap<u128, Vec<Span>>,
    /// Configuration
    config: BatchConfig,
    /// Metrics (sequential - no atomics)
    metrics: BatchMetrics,
    /// Last flush time
    last_flush: Instant,
}

impl BatchProcessor {
    /// Creates a new batch processor
    pub fn new(config: BatchConfig) -> Self {
        Self {
            pending: HashMap::new(),
            config,
            metrics: BatchMetrics::default(),
            last_flush: Instant::now(),
        }
    }

    /// Adds a span to the batch
    pub fn add(&mut self, span: Span) {
        let trace_spans = self.pending.entry(span.trace_id).or_default();
        trace_spans.push(span);
    }

    /// Returns the total number of pending spans
    pub fn total_pending(&self) -> usize {
        self.pending.values().map(|v| v.len()).sum()
    }

    /// Checks if the batch should be flushed
    pub fn should_flush(&self) -> bool {
        !self.pending.is_empty()
            && (self.total_pending() >= self.config.batch_size_limit
                || self.last_flush.elapsed() >= self.config.batch_timeout)
    }

    /// Flushes all pending spans using the provided exporter (sequential - blocks until export completes)
    ///
    /// The exporter is passed as a parameter to avoid storing `Arc` in the processor.
    pub async fn flush(&mut self, exporter: &dyn SpanExporterBoxed) -> Result<(), ExportError> {
        if self.pending.is_empty() {
            return Ok(());
        }

        // Collect all spans into a batch
        let spans: Vec<Span> = self
            .pending
            .drain()
            .flat_map(|(_, spans)| spans)
            .collect();

        let span_count = spans.len();
        let batch = SpanBatch::with_spans(spans);

        // Export the batch using the boxed method
        match exporter.export_boxed(batch).await {
            Ok(()) => {
                self.metrics.record_success(span_count as u64);
                self.last_flush = Instant::now();
                Ok(())
            }
            Err(e) => {
                self.metrics.record_error();
                Err(e)
            }
        }
    }

    /// Takes all pending spans as a batch (for concurrent export)
    ///
    /// Returns `None` if no spans are pending.
    /// The caller is responsible for exporting the batch and recording metrics.
    pub fn take_batch(&mut self) -> Option<SpanBatch> {
        if self.pending.is_empty() {
            return None;
        }

        let spans: Vec<Span> = self
            .pending
            .drain()
            .flat_map(|(_, spans)| spans)
            .collect();

        self.last_flush = Instant::now();
        Some(SpanBatch::with_spans(spans))
    }

    /// Returns current metrics (for sequential use)
    pub fn metrics(&self) -> &BatchMetrics {
        &self.metrics
    }

    /// Returns mutable metrics (for recording from external flush)
    pub fn metrics_mut(&mut self) -> &mut BatchMetrics {
        &mut self.metrics
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exporter::StdoutExporter;
    use crate::span::SpanKind;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_batch_processor_basic() {
        let exporter = Arc::new(StdoutExporter::new(false)); // Silent mode for tests
        let config = BatchConfig {
            batch_size_limit: 5,
            batch_timeout: Duration::from_secs(10),
        };
        let mut processor = BatchProcessor::new(config);

        // Add spans
        for i in 0..3 {
            let span = Span::new(1, i, 0, format!("op-{}", i), SpanKind::Internal);
            processor.add(span);
        }

        assert_eq!(processor.total_pending(), 3);
        assert!(!processor.should_flush()); // Below limit

        // Add more to trigger flush
        for i in 3..5 {
            let span = Span::new(1, i, 0, format!("op-{}", i), SpanKind::Internal);
            processor.add(span);
        }

        assert!(processor.should_flush()); // At limit

        processor.flush(exporter.as_ref()).await.unwrap();
        assert_eq!(processor.total_pending(), 0);
        assert_eq!(processor.metrics().spans_exported, 5);
        assert_eq!(processor.metrics().batches_exported, 1);
    }

    #[tokio::test]
    async fn test_batch_processor_multiple_traces() {
        let exporter = Arc::new(StdoutExporter::new(false));
        let config = BatchConfig::default();
        let mut processor = BatchProcessor::new(config);

        // Add spans from different traces
        for trace_id in 1..=3 {
            for span_id in 1..=2 {
                let span = Span::new(
                    trace_id,
                    span_id,
                    0,
                    format!("trace-{}-span-{}", trace_id, span_id),
                    SpanKind::Internal,
                );
                processor.add(span);
            }
        }

        assert_eq!(processor.total_pending(), 6);
        processor.flush(exporter.as_ref()).await.unwrap();
        assert_eq!(processor.metrics().spans_exported, 6);
    }

    #[tokio::test]
    async fn test_take_batch() {
        let config = BatchConfig::default();
        let mut processor = BatchProcessor::new(config);

        // Empty processor returns None
        assert!(processor.take_batch().is_none());

        // Add some spans
        for i in 0..5 {
            let span = Span::new(1, i, 0, format!("op-{}", i), SpanKind::Internal);
            processor.add(span);
        }

        // Take batch
        let batch = processor.take_batch();
        assert!(batch.is_some());
        assert_eq!(batch.unwrap().spans.len(), 5);

        // Processor is now empty
        assert_eq!(processor.total_pending(), 0);
        assert!(processor.take_batch().is_none());
    }
}
