use crate::exporter::{ExportError, SpanExporterBoxed};
use crate::span::{Span, SpanBatch};
use std::collections::HashMap;
use std::sync::Arc;
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

/// Metrics for batch processing
#[derive(Debug, Default, Clone)]
pub struct BatchMetrics {
    /// Total spans exported
    pub spans_exported: u64,
    /// Total batches exported
    pub batches_exported: u64,
    /// Total export errors
    pub export_errors: u64,
}

/// Batch processor that groups spans and exports them
pub struct BatchProcessor {
    /// Pending spans grouped by trace_id
    pending: HashMap<u128, Vec<Span>>,
    /// Configuration
    config: BatchConfig,
    /// Exporter for sending batches (uses boxed version for object safety)
    exporter: Arc<dyn SpanExporterBoxed>,
    /// Metrics
    metrics: BatchMetrics,
    /// Last flush time
    last_flush: Instant,
}

impl BatchProcessor {
    /// Creates a new batch processor
    pub fn new(config: BatchConfig, exporter: Arc<dyn SpanExporterBoxed>) -> Self {
        Self {
            pending: HashMap::new(),
            config,
            exporter,
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
        self.total_pending() >= self.config.batch_size_limit
            || self.last_flush.elapsed() >= self.config.batch_timeout
    }

    /// Flushes all pending spans
    pub async fn flush(&mut self) -> Result<(), ExportError> {
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
        match self.exporter.export_boxed(batch).await {
            Ok(()) => {
                self.metrics.spans_exported += span_count as u64;
                self.metrics.batches_exported += 1;
                self.last_flush = Instant::now();
                Ok(())
            }
            Err(e) => {
                self.metrics.export_errors += 1;
                Err(e)
            }
        }
    }

    /// Returns current metrics
    pub fn metrics(&self) -> &BatchMetrics {
        &self.metrics
    }

    /// Resets metrics
    pub fn reset_metrics(&mut self) {
        self.metrics = BatchMetrics::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exporter::StdoutExporter;
    use crate::span::SpanKind;

    #[tokio::test]
    async fn test_batch_processor_basic() {
        let exporter = Arc::new(StdoutExporter::new(false)); // Silent mode for tests
        let config = BatchConfig {
            batch_size_limit: 5,
            batch_timeout: Duration::from_secs(10),
        };
        let mut processor = BatchProcessor::new(config, exporter);

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

        processor.flush().await.unwrap();
        assert_eq!(processor.total_pending(), 0);
        assert_eq!(processor.metrics().spans_exported, 5);
        assert_eq!(processor.metrics().batches_exported, 1);
    }

    #[tokio::test]
    async fn test_batch_processor_multiple_traces() {
        let exporter = Arc::new(StdoutExporter::new(false));
        let config = BatchConfig::default();
        let mut processor = BatchProcessor::new(config, exporter);

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
        processor.flush().await.unwrap();
        assert_eq!(processor.metrics().spans_exported, 6);
    }
}
