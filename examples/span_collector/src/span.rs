use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::SystemTime;

/// Represents a single distributed tracing span
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Span {
    /// Unique trace identifier (128-bit)
    pub trace_id: u128,
    /// Unique span identifier (64-bit)
    pub span_id: u64,
    /// Parent span identifier (0 if root span)
    pub parent_span_id: u64,
    /// Span start time (Unix nanoseconds)
    pub start_time: u64,
    /// Span end time (Unix nanoseconds)
    pub end_time: u64,
    /// Operation name
    pub name: String,
    /// Span attributes (boxed to keep Span size manageable)
    pub attributes: Box<HashMap<String, AttributeValue>>,
    /// Span status
    pub status: SpanStatus,
    /// Span kind
    pub kind: SpanKind,
}

/// Attribute value types for span metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AttributeValue {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Array(Vec<String>),
}

/// Span execution status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpanStatus {
    /// Span completed successfully
    Ok,
    /// Span completed with error
    Error,
    /// Span status unknown
    Unset,
}

/// Span kind according to OpenTelemetry specification
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpanKind {
    /// Internal operation span
    Internal,
    /// Server-side RPC span
    Server,
    /// Client-side RPC span
    Client,
    /// Producer span (messaging)
    Producer,
    /// Consumer span (messaging)
    Consumer,
}

/// Batch of spans for export
#[derive(Debug)]
pub struct SpanBatch {
    /// All spans in this batch
    pub spans: Vec<Span>,
    /// Batch creation timestamp
    pub timestamp: SystemTime,
}

impl Span {
    /// Creates a new span with the given parameters
    pub fn new(
        trace_id: u128,
        span_id: u64,
        parent_span_id: u64,
        name: String,
        kind: SpanKind,
    ) -> Self {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;

        Self {
            trace_id,
            span_id,
            parent_span_id,
            start_time: now,
            end_time: now,
            name,
            attributes: Box::new(HashMap::new()),
            status: SpanStatus::Unset,
            kind,
        }
    }

    /// Marks the span as completed with the given status
    pub fn finish(&mut self, status: SpanStatus) {
        self.end_time = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        self.status = status;
    }

    /// Adds an attribute to the span
    pub fn set_attribute(&mut self, key: String, value: AttributeValue) {
        self.attributes.insert(key, value);
    }

    /// Duration of the span in nanoseconds
    pub fn duration_nanos(&self) -> u64 {
        self.end_time.saturating_sub(self.start_time)
    }
}

impl SpanBatch {
    /// Creates a new empty span batch
    pub fn new() -> Self {
        Self {
            spans: Vec::new(),
            timestamp: SystemTime::now(),
        }
    }

    /// Creates a batch with the given spans
    pub fn with_spans(spans: Vec<Span>) -> Self {
        Self {
            spans,
            timestamp: SystemTime::now(),
        }
    }

    /// Adds a span to the batch
    pub fn add(&mut self, span: Span) {
        self.spans.push(span);
    }

    /// Returns the number of spans in the batch
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// Returns true if the batch is empty
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
}

impl Default for SpanBatch {
    fn default() -> Self {
        Self::new()
    }
}
