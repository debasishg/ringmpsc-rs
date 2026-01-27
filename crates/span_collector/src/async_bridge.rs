use crate::batch_processor::{BatchConfig, BatchProcessor};
use crate::collector::{CollectorConfig, CollectorMetrics, SpanCollector, SubmitError};
use crate::exporter::{ExportError, SpanExporterBoxed};
use crate::span::{Span, SpanBatch};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{oneshot, Notify, Semaphore};
use tokio::task::{JoinHandle, JoinSet};

/// Error types for async operations
#[derive(Debug, Error)]
pub enum AsyncError {
    /// Error during registration
    #[error("registration failed: {0}")]
    RegistrationFailed(String),
    /// Error during export
    #[error("export failed: {0}")]
    ExportFailed(#[from] ExportError),
    /// Channel is closed
    #[error("channel is closed")]
    Closed,
}

/// Thread-safe metrics for concurrent exports (uses atomics)
///
/// This is separate from `BatchMetrics` which uses plain u64 for sequential use.
/// Keeping them separate avoids atomic overhead when concurrency isn't needed.
#[derive(Debug, Default)]
pub struct ExportMetrics {
    /// Total spans exported across all concurrent tasks
    pub spans_exported: AtomicU64,
    /// Total batches exported
    pub batches_exported: AtomicU64,
    /// Total export errors
    pub export_errors: AtomicU64,
    /// Current in-flight exports
    pub inflight_exports: AtomicU64,
}

impl ExportMetrics {
    pub fn spans_exported(&self) -> u64 {
        self.spans_exported.load(Ordering::Relaxed)
    }

    pub fn batches_exported(&self) -> u64 {
        self.batches_exported.load(Ordering::Relaxed)
    }

    pub fn export_errors(&self) -> u64 {
        self.export_errors.load(Ordering::Relaxed)
    }

    pub fn inflight_exports(&self) -> u64 {
        self.inflight_exports.load(Ordering::Relaxed)
    }

    fn record_success(&self, span_count: u64) {
        self.spans_exported.fetch_add(span_count, Ordering::Relaxed);
        self.batches_exported.fetch_add(1, Ordering::Relaxed);
    }

    fn record_error(&self) {
        self.export_errors.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_inflight(&self) {
        self.inflight_exports.fetch_add(1, Ordering::Relaxed);
    }

    fn dec_inflight(&self) {
        self.inflight_exports.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Configuration for the async span collector
#[derive(Debug, Clone)]
pub struct AsyncCollectorConfig {
    /// Collector configuration
    pub collector_config: CollectorConfig,
    /// Batch configuration
    pub batch_config: BatchConfig,
    /// Consumer polling interval
    pub consumer_interval: Duration,
    /// Maximum spans to consume per poll
    pub max_consume_per_poll: usize,
    /// Maximum concurrent export operations
    pub max_concurrent_exports: usize,
}

impl Default for AsyncCollectorConfig {
    fn default() -> Self {
        Self {
            collector_config: CollectorConfig::default(),
            batch_config: BatchConfig::default(),
            consumer_interval: Duration::from_millis(100), // 10Hz
            max_consume_per_poll: 10_000,
            max_concurrent_exports: 4,
        }
    }
}

/// Helper function to export a batch and record metrics (for concurrent tasks)
async fn export_batch(
    exporter: Arc<dyn SpanExporterBoxed>,
    batch: SpanBatch,
    metrics: &ExportMetrics,
    span_count: u64,
) -> Result<(), ExportError> {
    match exporter.export_boxed(batch).await {
        Ok(()) => {
            metrics.record_success(span_count);
            Ok(())
        }
        Err(e) => {
            metrics.record_error();
            Err(e)
        }
    }
}

/// Async span collector that bridges sync MPSC channels with async Rust
pub struct AsyncSpanCollector {
    collector: Arc<SpanCollector>,
    consumer_task: Option<JoinHandle<()>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    backpressure_notify: Arc<Notify>,
    export_metrics: Arc<ExportMetrics>,
}


impl AsyncSpanCollector {
    /// Creates a new async span collector with the given configuration and exporter
    ///
    /// Uses `SpanExporterBoxed` for object safety with native async traits.
    /// Supports concurrent exports using a Semaphore to limit parallelism.
    pub async fn new(config: AsyncCollectorConfig, exporter: Arc<dyn SpanExporterBoxed>) -> Self {
        let collector = Arc::new(SpanCollector::new(config.collector_config));
        let backpressure_notify = Arc::new(Notify::new());
        let export_metrics = Arc::new(ExportMetrics::default());

        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

        // Spawn consumer task with concurrent export support
        let collector_clone = Arc::clone(&collector);
        let backpressure_clone = Arc::clone(&backpressure_notify);
        let metrics_clone = Arc::clone(&export_metrics);
        let consumer_interval = config.consumer_interval;
        let max_consume = config.max_consume_per_poll;
        let max_concurrent = config.max_concurrent_exports;

        let consumer_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(consumer_interval);
            // BatchProcessor is pure batching - no Arc, no atomics
            let mut batch_processor = BatchProcessor::new(config.batch_config);

            // Semaphore to limit concurrent exports
            let export_semaphore = Arc::new(Semaphore::new(max_concurrent));
            // JoinSet to track in-flight export tasks
            let mut export_tasks: JoinSet<Result<(), ExportError>> = JoinSet::new();

            loop {
                tokio::select! {
                    // Reap completed export tasks (non-blocking)
                    Some(result) = export_tasks.join_next(), if !export_tasks.is_empty() => {
                        match result {
                            Ok(Ok(())) => {
                                // Export succeeded - metrics already recorded in task
                            }
                            Ok(Err(e)) => {
                                eprintln!("Export error: {}", e);
                            }
                            Err(e) => {
                                eprintln!("Export task panicked: {}", e);
                            }
                        }
                    }

                    _ = interval.tick() => {
                        // Consume spans from ring buffers (zero-copy transfer)
                        let consumed = collector_clone.consume_all_up_to(max_consume, |span| {
                            batch_processor.add(span);
                        });

                        if consumed > 0 {
                            // Notify waiting producers that space is available
                            backpressure_clone.notify_waiters();
                        }

                        // Spawn concurrent export if batch is ready
                        if batch_processor.should_flush() {
                            if let Some(batch) = batch_processor.take_batch() {
                                let permit = export_semaphore.clone().acquire_owned().await;
                                if let Ok(permit) = permit {
                                    let exporter = Arc::clone(&exporter);
                                    let metrics = Arc::clone(&metrics_clone);
                                    let span_count = batch.spans.len() as u64;

                                    metrics.inc_inflight();

                                    export_tasks.spawn(async move {
                                        let result = export_batch(exporter, batch, &metrics, span_count).await;
                                        metrics.dec_inflight();
                                        drop(permit); // Release semaphore
                                        result
                                    });
                                }
                            }
                        }
                    }

                    _ = &mut shutdown_rx => {
                        // Drain remaining spans (zero-copy transfer)
                        collector_clone.consume_all(|span| {
                            batch_processor.add(span);
                        });

                        // Final flush - sequential, pass exporter directly
                        if let Some(batch) = batch_processor.take_batch() {
                            if let Err(e) = exporter.export_boxed(batch).await {
                                eprintln!("Final export error: {}", e);
                            }
                        }

                        // Wait for all in-flight exports to complete
                        while let Some(result) = export_tasks.join_next().await {
                            if let Ok(Err(e)) = result {
                                eprintln!("In-flight export error during shutdown: {}", e);
                            }
                        }

                        break;
                    }
                }
            }
        });

        Self {
            collector,
            consumer_task: Some(consumer_task),
            shutdown_tx: Some(shutdown_tx),
            backpressure_notify,
            export_metrics,
        }
    }

    /// Registers a new async span producer
    pub async fn register_producer(&self) -> Result<AsyncSpanProducer, AsyncError> {
        let producer = self.collector.register()
            .map_err(|e| AsyncError::RegistrationFailed(e.to_string()))?;
        Ok(AsyncSpanProducer {
            producer,
            backpressure_notify: Arc::clone(&self.backpressure_notify),
        })
    }

    /// Returns collector metrics
    pub fn metrics(&self) -> &Arc<CollectorMetrics> {
        self.collector.metrics()
    }

    /// Returns export metrics (thread-safe for concurrent exports)
    ///
    /// These metrics track spans/batches exported across concurrent tasks.
    pub fn export_metrics(&self) -> &Arc<ExportMetrics> {
        &self.export_metrics
    }

    /// Gracefully shuts down the collector
    pub async fn shutdown(mut self) -> Result<(), AsyncError> {
        // Close the collector to prevent new registrations
        self.collector.close();

        // Send shutdown signal to consumer task
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }

        // Wait for consumer task to finish
        if let Some(task) = self.consumer_task.take() {
            task.await.map_err(|e| {
                AsyncError::ExportFailed(ExportError::Transport(format!("task join error: {}", e)))
            })?;
        }

        Ok(())
    }
}

/// Async handle for submitting spans
pub struct AsyncSpanProducer {
    producer: crate::collector::SpanProducer,
    backpressure_notify: Arc<Notify>,
}

impl AsyncSpanProducer {
    /// Submits a span, waiting if the ring buffer is full
    pub async fn submit_span(&self, span: Span) -> Result<(), SubmitError> {
        loop {
            match self.producer.try_submit_span(span.clone()) {
                Ok(()) => return Ok(()),
                Err(SubmitError::Full) => {
                    // Ring full - wait for consumer to drain
                    self.backpressure_notify.notified().await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Tries to submit a span without blocking
    pub fn try_submit_span(&self, span: Span) -> Result<(), SubmitError> {
        self.producer.try_submit_span(span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exporter::TestExporter;
    use crate::span::SpanKind;

    #[tokio::test]
    async fn test_async_collector_basic() {
        let exporter = Arc::new(TestExporter::new());
        let config = AsyncCollectorConfig {
            consumer_interval: Duration::from_millis(50),
            ..Default::default()
        };
        let collector = AsyncSpanCollector::new(config, exporter.clone()).await;

        let producer = collector.register_producer().await.unwrap();

        // Submit spans
        for i in 0..10 {
            let span = Span::new(1, i, 0, format!("op-{}", i), SpanKind::Internal);
            producer.submit_span(span).await.unwrap();
        }

        // Wait for consumer to process
        tokio::time::sleep(Duration::from_millis(200)).await;

        collector.shutdown().await.unwrap();

        // Verify all spans were exported
        assert_eq!(exporter.exported_count(), 10);
    }

    #[tokio::test]
    async fn test_async_multiple_producers() {
        let exporter = Arc::new(TestExporter::new());
        let config = AsyncCollectorConfig::default();
        let collector = Arc::new(AsyncSpanCollector::new(config, exporter.clone()).await);

        let mut tasks = vec![];
        for producer_id in 0..4 {
            let collector_clone = Arc::clone(&collector);
            let task = tokio::spawn(async move {
                let producer = collector_clone.register_producer().await.unwrap();
                for seq in 0..100 {
                    let span = Span::new(
                        1,
                        (producer_id as u64) << 48 | seq,
                        0,
                        format!("op-{}", seq),
                        SpanKind::Internal,
                    );
                    producer.submit_span(span).await.unwrap();
                }
            });
            tasks.push(task);
        }

        for task in tasks {
            task.await.unwrap();
        }

        // Wait for processing
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Unwrap Arc to get ownership
        let collector = match Arc::try_unwrap(collector) {
            Ok(c) => c,
            Err(_) => panic!("Failed to unwrap Arc"),
        };
        collector.shutdown().await.unwrap();

        assert_eq!(exporter.exported_count(), 400);
    }

    #[tokio::test]
    async fn test_async_graceful_shutdown() {
        let exporter = Arc::new(TestExporter::new());
        let config = AsyncCollectorConfig::default();
        let collector = AsyncSpanCollector::new(config, exporter.clone()).await;

        let producer = collector.register_producer().await.unwrap();

        // Submit spans
        for i in 0..100 {
            let span = Span::new(1, i, 0, format!("op-{}", i), SpanKind::Internal);
            producer.submit_span(span).await.unwrap();
        }

        // Shutdown immediately (should drain all spans)
        collector.shutdown().await.unwrap();

        assert_eq!(exporter.exported_count(), 100);
    }
}
