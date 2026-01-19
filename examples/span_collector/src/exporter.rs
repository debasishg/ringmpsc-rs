use crate::span::SpanBatch;
use async_trait::async_trait;
use thiserror::Error;

/// Error types for span export operations
#[derive(Debug, Error)]
pub enum ExportError {
    /// Transport-layer error (network, gRPC, HTTP)
    #[error("transport error: {0}")]
    Transport(String),
    /// Serialization error
    #[error("serialization error: {0}")]
    Serialization(String),
    /// All retry attempts exhausted
    #[error("all retry attempts exhausted")]
    RetriesExhausted,
    /// Export operation timed out
    #[error("export operation timed out")]
    Timeout,
}

/// Trait for exporting span batches to various backends
/// We need `#[async_trait]` because the code does use dynamic dispatch - 
/// there are multiple places using `Arc<dyn SpanExporter>`
#[async_trait]
pub trait SpanExporter: Send + Sync {
    /// Exports a batch of spans
    async fn export(&self, batch: SpanBatch) -> Result<(), ExportError>;

    /// Returns the exporter name for debugging
    fn name(&self) -> &str;
}

/// Stdout exporter for testing and debugging
pub struct StdoutExporter {
    verbose: bool,
}

impl StdoutExporter {
    /// Creates a new stdout exporter
    pub fn new(verbose: bool) -> Self {
        Self { verbose }
    }
}

#[async_trait]
impl SpanExporter for StdoutExporter {
    async fn export(&self, batch: SpanBatch) -> Result<(), ExportError> {
        if self.verbose {
            println!("=== Exporting {} spans ===", batch.spans.len());
            for span in &batch.spans {
                println!(
                    "Span: trace_id={:032x} span_id={:016x} name={} duration={}ns status={:?}",
                    span.trace_id,
                    span.span_id,
                    span.name,
                    span.duration_nanos(),
                    span.status
                );
            }
            println!("=== Export complete ===\n");
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "stdout"
    }
}

/// JSON file exporter for local development
pub struct JsonFileExporter {
    file_path: String,
}

impl JsonFileExporter {
    /// Creates a new JSON file exporter
    pub fn new(file_path: String) -> Self {
        Self { file_path }
    }
}

#[async_trait]
impl SpanExporter for JsonFileExporter {
    async fn export(&self, batch: SpanBatch) -> Result<(), ExportError> {
        let json = serde_json::to_string_pretty(&batch.spans)
            .map_err(|e| ExportError::Serialization(e.to_string()))?;

        tokio::fs::write(&self.file_path, json)
            .await
            .map_err(|e| ExportError::Transport(e.to_string()))?;

        Ok(())
    }

    fn name(&self) -> &str {
        "json_file"
    }
}

/// Null exporter that discards all spans (for benchmarking)
pub struct NullExporter;

impl NullExporter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for NullExporter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SpanExporter for NullExporter {
    async fn export(&self, _batch: SpanBatch) -> Result<(), ExportError> {
        // Discard all spans
        Ok(())
    }

    fn name(&self) -> &str {
        "null"
    }
}

/// Test exporter that records all exported spans for verification
#[cfg(test)]
pub struct TestExporter {
    spans: std::sync::Mutex<Vec<crate::span::Span>>,
}

#[cfg(test)]
impl Default for TestExporter {
    fn default() -> Self {
        Self {
            spans: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[cfg(test)]
impl TestExporter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn exported_count(&self) -> usize {
        self.spans.lock().unwrap().len()
    }

    pub fn spans_by_producer(&self, producer_id: usize) -> Vec<crate::span::Span> {
        self.spans
            .lock()
            .unwrap()
            .iter()
            .filter(|s| (s.span_id >> 48) as usize == producer_id)
            .cloned()
            .collect()
    }

    pub fn all_spans(&self) -> Vec<crate::span::Span> {
        self.spans.lock().unwrap().clone()
    }
}

#[cfg(test)]
#[async_trait]
impl SpanExporter for TestExporter {
    async fn export(&self, batch: SpanBatch) -> Result<(), ExportError> {
        self.spans.lock().unwrap().extend(batch.spans);
        Ok(())
    }

    fn name(&self) -> &str {
        "test"
    }
}

/// Slow exporter for backpressure testing
#[cfg(test)]
pub struct SlowExporter {
    delay: std::time::Duration,
    spans: std::sync::Mutex<Vec<crate::span::Span>>,
}

#[cfg(test)]
impl SlowExporter {
    pub fn new(delay: std::time::Duration) -> Self {
        Self {
            delay,
            spans: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn exported_count(&self) -> usize {
        self.spans.lock().unwrap().len()
    }
}

#[cfg(test)]
#[async_trait]
impl SpanExporter for SlowExporter {
    async fn export(&self, batch: SpanBatch) -> Result<(), ExportError> {
        tokio::time::sleep(self.delay).await;
        self.spans.lock().unwrap().extend(batch.spans);
        Ok(())
    }

    fn name(&self) -> &str {
        "slow"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::{Span, SpanKind};

    #[tokio::test]
    async fn test_stdout_exporter() {
        let exporter = StdoutExporter::new(false);
        let mut batch = SpanBatch::new();
        batch.add(Span::new(1, 1, 0, "test".to_string(), SpanKind::Internal));

        let result = exporter.export(batch).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_null_exporter() {
        let exporter = NullExporter::new();
        let mut batch = SpanBatch::new();
        for i in 0..1000 {
            batch.add(Span::new(i as u128, i, 0, "test".to_string(), SpanKind::Internal));
        }

        let result = exporter.export(batch).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_test_exporter() {
        let exporter = TestExporter::new();
        let mut batch = SpanBatch::new();
        
        for i in 0..10 {
            batch.add(Span::new(i as u128, i, 0, "test".to_string(), SpanKind::Internal));
        }

        exporter.export(batch).await.unwrap();
        assert_eq!(exporter.exported_count(), 10);
    }
}
