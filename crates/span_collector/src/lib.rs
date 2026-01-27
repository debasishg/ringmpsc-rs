//! OpenTelemetry Span Collector
//!
//! A high-performance distributed tracing span collector that combines ringmpsc-rs's
//! lock-free MPSC channels with async Rust. Enables instrumented services to submit
//! spans with <100ns latency while batching exports to tracing backends.
//!
//! # Rust 2024 Edition Features
//!
//! This crate uses native async traits (no `#[async_trait]` macro), making it a good
//! example of modern async Rust patterns.

pub mod async_bridge;
pub mod batch_processor;
pub mod collector;
pub mod exporter;
pub mod rate_limiter;
pub mod resilient_exporter;
pub mod span;

// Re-export main types
pub use async_bridge::{AsyncCollectorConfig, AsyncSpanCollector, AsyncSpanProducer, ExportMetrics};
pub use batch_processor::{BatchConfig, BatchMetrics, BatchProcessor};
pub use collector::{CollectorConfig, CollectorMetrics, SpanCollector, SpanProducer, SubmitError};
pub use exporter::{ExportError, JsonFileExporter, NullExporter, SpanExporter, SpanExporterBoxed, StdoutExporter};
pub use rate_limiter::{IntervalRateLimiter, RateLimiter, RateLimiterBoxed};
pub use resilient_exporter::{
    CircuitBreakerConfig, CircuitBreakerExporter, CircuitState, RateLimitedExporter,
    ResilientExporterBuilder, RetryConfig, RetryingExporter,
};
pub use span::{AttributeValue, Span, SpanBatch, SpanKind, SpanStatus};
