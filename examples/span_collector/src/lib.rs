//! OpenTelemetry Span Collector
//!
//! A high-performance distributed tracing span collector that combines ringmpsc-rs's
//! lock-free MPSC channels with async Rust. Enables instrumented services to submit
//! spans with <100ns latency while batching exports to tracing backends.

pub mod async_bridge;
pub mod batch_processor;
pub mod collector;
pub mod exporter;
pub mod span;

// Re-export main types
pub use async_bridge::{AsyncCollectorConfig, AsyncSpanCollector, AsyncSpanProducer};
pub use batch_processor::{BatchConfig, BatchProcessor};
pub use collector::{CollectorConfig, SpanCollector, SpanProducer, SubmitError};
pub use exporter::{ExportError, SpanExporter, StdoutExporter, JsonFileExporter, NullExporter};
pub use span::{AttributeValue, Span, SpanBatch, SpanKind, SpanStatus};
