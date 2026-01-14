use crate::batch_processor::{BatchConfig, BatchProcessor};
use crate::collector::{CollectorConfig, CollectorMetrics, SpanCollector, SubmitError};
use crate::exporter::{ExportError, SpanExporter};
use crate::span::Span;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Notify};
use tokio::task::JoinHandle;

/// Error types for async operations
#[derive(Debug)]
pub enum AsyncError {
    /// Error during registration
    RegistrationFailed(String),
    /// Error during export
    ExportFailed(ExportError),
    /// Channel is closed
    Closed,
}

impl std::fmt::Display for AsyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AsyncError::RegistrationFailed(e) => write!(f, "registration failed: {}", e),
            AsyncError::ExportFailed(e) => write!(f, "export failed: {}", e),
            AsyncError::Closed => write!(f, "channel is closed"),
        }
    }
}

impl std::error::Error for AsyncError {}

impl From<ExportError> for AsyncError {
    fn from(e: ExportError) -> Self {
        AsyncError::ExportFailed(e)
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
}

impl Default for AsyncCollectorConfig {
    fn default() -> Self {
        Self {
            collector_config: CollectorConfig::default(),
            batch_config: BatchConfig::default(),
            consumer_interval: Duration::from_millis(100), // 10Hz
            max_consume_per_poll: 10_000,
        }
    }
}

/// Async span collector that bridges sync MPSC channels with async Rust
pub struct AsyncSpanCollector {
    collector: Arc<SpanCollector>,
    consumer_task: Option<JoinHandle<()>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    backpressure_notify: Arc<Notify>,
}

impl AsyncSpanCollector {
    /// Creates a new async span collector with the given configuration and exporter
    pub async fn new(config: AsyncCollectorConfig, exporter: Arc<dyn SpanExporter>) -> Self {
        let collector = Arc::new(SpanCollector::new(config.collector_config));
        let backpressure_notify = Arc::new(Notify::new());

        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

        // Spawn consumer task
        let collector_clone = Arc::clone(&collector);
        let backpressure_clone = Arc::clone(&backpressure_notify);
        let consumer_interval = config.consumer_interval;
        let max_consume = config.max_consume_per_poll;

        let consumer_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(consumer_interval);
            let mut batch_processor = BatchProcessor::new(config.batch_config, exporter);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // Consume spans from ring buffers
                        let consumed = collector_clone.consume_all_up_to(max_consume, |span| {
                            batch_processor.add(span.clone());
                        });

                        if consumed > 0 {
                            // Notify waiting producers that space is available
                            backpressure_clone.notify_waiters();
                        }

                        // Flush batch if needed
                        if batch_processor.should_flush() {
                            if let Err(e) = batch_processor.flush().await {
                                eprintln!("Export error: {}", e);
                            }
                        }
                    }
                    _ = &mut shutdown_rx => {
                        // Drain remaining spans
                        collector_clone.consume_all(|span| {
                            batch_processor.add(span.clone());
                        });

                        // Final flush
                        if let Err(e) = batch_processor.flush().await {
                            eprintln!("Final export error: {}", e);
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
