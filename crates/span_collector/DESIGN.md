# OpenTelemetry Span Collector Design

## Overview

A high-performance distributed tracing span collector that combines ringmpsc-rs's lock-free MPSC channels with async Rust. Enables instrumented services to submit spans with <100ns latency while batching exports to tracing backends (Jaeger/Zipkin/OTLP).

**Rust 2024 Edition**: This crate uses native async traits (no `#[async_trait]` macro), with `SpanExporterBoxed` and `RateLimiterBoxed` wrapper traits for object-safe dynamic dispatch.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    Instrumented Service                      │
├─────────────────────────────────────────────────────────────┤
│  Async Task 1  │  Async Task 2  │ ... │  Async Task N      │
│  (Producer)    │  (Producer)    │     │  (Producer)        │
└────────┬────────────────┬─────────────────────┬─────────────┘
         │                │                     │
         │ submit_span()  │ submit_span()      │ submit_span()
         │ <100ns         │ <100ns             │ <100ns
         ▼                ▼                     ▼
┌─────────────────────────────────────────────────────────────┐
│              Lock-Free MPSC Ring Buffers                     │
│  Ring 0 (4K)  │  Ring 1 (4K)  │ ... │  Ring N (4K)         │
│  [Span|Span]  │  [Span|Span]  │     │  [Span|Span]         │
└────────┬────────────────┴─────────────────────┴─────────────┘
         │
         │ consume_all_up_to(10K)
         │ tokio::spawn consumer task
         ▼
┌─────────────────────────────────────────────────────────────┐
│                   Async Batch Processor                      │
│  • Group spans by trace_id                                   │
│  • 5s timeout or 10K span batch limit                        │
│  • Backpressure via tokio::sync::Notify                      │
└────────┬────────────────────────────────────────────────────┘
         │
         │ export_batch()
         ▼
┌─────────────────────────────────────────────────────────────┐
│                     Span Exporter                            │
│  • OTLP gRPC (tonic) - primary                               │
│  • OTLP HTTP (reqwest) - fallback                            │
│  • Retry with exponential backoff                            │
└─────────────────────────────────────────────────────────────┘
```

## Components

### 1. Span Data Model (`span.rs`)

```rust
pub struct Span {
    pub trace_id: u128,           // 16 bytes
    pub span_id: u64,             // 8 bytes
    pub parent_span_id: u64,      // 8 bytes
    pub start_time: u64,          // Unix nanos
    pub end_time: u64,            // Unix nanos
    pub name: String,             // Operation name
    pub attributes: Box<HashMap<String, AttributeValue>>, // Boxed to keep Span small
    pub status: SpanStatus,       // Ok/Error
    pub kind: SpanKind,           // Client/Server/Internal/Producer/Consumer
}
```

**Size optimization**: ~88 bytes base + boxed attributes. Fits efficiently in 4K rings (4096 / 88 = 46 spans).

**Traits**:
- `From<Span>` for `opentelemetry_proto::trace::v1::Span` (OTLP conversion)
- `SpanBatch` struct groups related spans for efficient export

### 2. Sync Collector Core (`collector.rs`)

```rust
pub struct SpanCollector {
    channel: Arc<Channel<Span>>,
    config: CollectorConfig,
    metrics: CollectorMetrics,
}

pub struct SpanProducer {
    producer: Producer<Span>,
    backoff: Backoff,
}

impl SpanProducer {
    pub fn submit_span(&self, span: Span) -> Result<(), SubmitError> {
        // Fast path: reserve(1) with retry
        // Uses reserve_with_backoff() from reservation.rs
        // Returns Err(SubmitError::Full) after backoff exhausted
    }
}
```

**Configuration** (using `LOW_LATENCY_CONFIG`):
- `ring_bits = 12` (4096 spans per ring)
- `max_producers = 16` (typical microservice has 4-8 async runtime threads)
- `enable_metrics = true` (track spans_submitted, full_ring_events)

**Memory footprint**: 16 rings × 4K × 88 bytes ≈ 5.6 MB

### 3. Async Bridge Layer (`async_bridge.rs`)

```rust
pub struct AsyncSpanCollector {
    collector: Arc<SpanCollector>,
    consumer_task: JoinHandle<()>,
    shutdown_tx: oneshot::Sender<()>,
    backpressure_notify: Arc<Notify>,
}

impl AsyncSpanCollector {
    pub async fn new(config: CollectorConfig, exporter: Arc<dyn SpanExporter>) -> Self {
        // Spawn consumer task
        let consumer_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // Consume up to 10K spans (prevents long pauses)
                        let consumed = collector.consume_all_up_to(10_000, |span| {
                            batch_processor.add(span);
                        });
                        if consumed > 0 {
                            backpressure_notify.notify_waiters();
                        }
                    }
                    _ = shutdown_rx.recv() => break,
                }
            }
        });
    }

    pub async fn register_producer(&self) -> Result<AsyncSpanProducer, RegisterError> {
        // Wraps sync Producer with async backpressure handling
    }

    pub async fn shutdown(self) -> Result<(), ExportError> {
        // Graceful shutdown: close channel, drain remaining spans, stop consumer
    }
}

pub struct AsyncSpanProducer {
    producer: SpanProducer,
    backpressure_notify: Arc<Notify>,
}

impl AsyncSpanProducer {
    pub async fn submit_span(&self, span: Span) -> Result<(), SubmitError> {
        loop {
            match self.producer.submit_span(span.clone()) {
                Ok(()) => return Ok(()),
                Err(SubmitError::Full) => {
                    // Ring full - wait for consumer to drain
                    self.backpressure_notify.notified().await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}
```

**Key patterns**:
- `tokio::spawn` for consumer task (async runtime integration)
- `tokio::time::interval` for periodic polling (100ms = 10Hz)
- `tokio::sync::Notify` for backpressure coordination (wake waiting producers)
- `tokio::sync::oneshot` for shutdown signaling

### 4. Batch Processor (`batch_processor.rs`)

```rust
pub struct BatchProcessor {
    pending: HashMap<u128, Vec<Span>>,  // Group by trace_id
    batch_timeout: Duration,             // 5s default
    batch_size_limit: usize,             // 10K spans
    exporter: Arc<dyn SpanExporter>,
}

impl BatchProcessor {
    pub fn add(&mut self, span: Span) {
        let trace_spans = self.pending.entry(span.trace_id).or_default();
        trace_spans.push(span);

        // Export if batch limit reached
        if self.total_pending() >= self.batch_size_limit {
            self.flush();
        }
    }

    pub async fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }

        let batch = SpanBatch {
            spans: self.pending.drain().flat_map(|(_, v)| v).collect(),
            timestamp: SystemTime::now(),
        };

        match self.exporter.export(batch).await {
            Ok(_) => self.metrics.spans_exported += batch.spans.len(),
            Err(e) => self.metrics.export_errors += 1,
        }
    }
}
```

**Batching strategy**:
- Group by `trace_id` for backend query efficiency
- Time-based flush (5s) OR size-based (10K spans)
- Async export with retry logic in exporter layer

### 5. Span Exporter (`exporter.rs`)

```rust
// Native async trait (Rust 2024 edition - no #[async_trait] macro)
pub trait SpanExporter: Send + Sync {
    fn export(&self, batch: SpanBatch) -> impl Future<Output = Result<(), ExportError>> + Send;
    fn name(&self) -> &str;
}

// Object-safe wrapper for dynamic dispatch
pub trait SpanExporterBoxed: Send + Sync {
    fn export_boxed(&self, batch: SpanBatch) 
        -> Pin<Box<dyn Future<Output = Result<(), ExportError>> + Send + '_>>;
    fn name(&self) -> &str;
}

// Blanket impl: any SpanExporter can be used as SpanExporterBoxed
impl<T: SpanExporter> SpanExporterBoxed for T { ... }
```

### 6. Resilient Exporters (`resilient_exporter.rs`)

```rust
/// Retry with exponential backoff
pub struct RetryingExporter<E> {
    inner: E,
    config: RetryConfig,  // max_retries, initial_delay, max_delay, backoff_multiplier
    total_retries: AtomicU64,
    recovered_exports: AtomicU64,
}

/// Circuit breaker pattern - fail fast when backend unhealthy
pub struct CircuitBreakerExporter<E> {
    inner: E,
    config: CircuitBreakerConfig,  // failure_threshold, reset_timeout, success_threshold
    state: Mutex<CircuitBreakerState>,  // Closed -> Open -> HalfOpen -> Closed
    times_opened: AtomicU32,
}

/// Rate limiting wrapper
pub struct RateLimitedExporter<E, R> {
    inner: E,
    rate_limiter: tokio::sync::Mutex<R>,  // async Mutex to hold across await
}

/// Builder for composing resilience patterns
pub struct ResilientExporterBuilder<E> { ... }
```

**Resilience patterns**:
- **Retry**: Exponential backoff (100ms → 200ms → 400ms), max 3 attempts by default
- **Circuit Breaker**: Opens after 5 failures, half-open after 30s, closes after 2 successes
- **Rate Limiting**: Control export rate using `RateLimiter` trait implementations

### 7. Rate Limiter (`rate_limiter.rs`)

```rust
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create collector with stdout exporter
    let exporter = Arc::new(StdoutExporter::new());
    let config = CollectorConfig::default();
    let collector = AsyncSpanCollector::new(config, exporter).await;

    // Spawn 8 producer tasks (simulating microservice threads)
    let mut producers = vec![];
    for i in 0..8 {
        let producer = collector.register_producer().await?;
        let task = tokio::spawn(async move {
            generate_spans(i, producer).await;
        });
        producers.push(task);
    }

    // Wait for producers to finish
    for task in producers {
        task.await?;
    }

    // Graceful shutdown
    collector.shutdown().await?;

    // Print metrics
    println!("Spans submitted: {}", collector.metrics().spans_submitted);
    println!("Spans exported: {}", collector.metrics().spans_exported);
    Ok(())
}

async fn generate_spans(producer_id: usize, producer: AsyncSpanProducer) {
    let mut rng = rand::thread_rng();
    for i in 0..50_000 {
        let span = Span {
            trace_id: rng.gen(),
            span_id: (producer_id as u64) << 48 | i,
            name: format!("operation-{}", i % 10),
            start_time: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64,
            // ... fill other fields ...
        };

        producer.submit_span(span).await.unwrap();

        // Simulate varying rates
        if i % 100 == 0 {
            tokio::time::sleep(Duration::from_micros(100)).await;
        }
    }
}
```

**Workload simulation**:
- 8 async tasks (producers) × 50K spans = 400K total
- Varying submission rates (burst + steady)
- Demonstrates backpressure handling under load

### 7. Integration Tests (`tests.rs`)

```rust
#[tokio::test]
async fn test_concurrent_span_submission() {
    let exporter = Arc::new(TestExporter::new());
    let collector = AsyncSpanCollector::new(
        CollectorConfig::default(),
        exporter.clone()
    ).await;

    // 16 producers × 25K spans = 400K total
    let mut tasks = vec![];
    for producer_id in 0..16 {
        let producer = collector.register_producer().await.unwrap();
        let task = tokio::spawn(async move {
            for seq in 0..25_000 {
                let span = Span {
                    span_id: (producer_id as u64) << 48 | seq,
                    // ... other fields ...
                };
                producer.submit_span(span).await.unwrap();
            }
        });
        tasks.push(task);
    }

    for task in tasks {
        task.await.unwrap();
    }

    collector.shutdown().await.unwrap();

    // Verify no data loss
    assert_eq!(exporter.exported_count(), 400_000);

    // Verify per-producer FIFO ordering
    for producer_id in 0..16 {
        let producer_spans = exporter.spans_by_producer(producer_id);
        for window in producer_spans.windows(2) {
            assert!(window[0].span_id < window[1].span_id);
        }
    }
}

#[tokio::test]
async fn test_backpressure() {
    // Small ring to force backpressure
    let config = CollectorConfig {
        ring_bits: 8,  // 256 spans
        ..Default::default()
    };
    // Slow exporter that blocks
    let exporter = Arc::new(SlowExporter::new(Duration::from_millis(100)));
    let collector = AsyncSpanCollector::new(config, exporter).await;

    // Fast producer should block gracefully (not error)
    let producer = collector.register_producer().await.unwrap();
    for i in 0..10_000 {
        producer.submit_span(create_test_span(i)).await.unwrap();
    }

    collector.shutdown().await.unwrap();
    // All spans eventually exported despite backpressure
    assert_eq!(exporter.exported_count(), 10_000);
}

#[tokio::test]
async fn test_graceful_shutdown_with_inflight_spans() {
    let exporter = Arc::new(TestExporter::new());
    let collector = AsyncSpanCollector::new(
        CollectorConfig::default(),
        exporter.clone()
    ).await;

    let producer = collector.register_producer().await.unwrap();
    
    // Submit spans
    for i in 0..1000 {
        producer.submit_span(create_test_span(i)).await.unwrap();
    }

    // Shutdown should drain all spans
    collector.shutdown().await.unwrap();
    
    assert_eq!(exporter.exported_count(), 1000);
}
```

**Test coverage**:
- Concurrent multi-producer submission (matches integration_tests.rs pattern)
- Per-producer FIFO ordering validation
- Backpressure handling (small rings, slow exporters)
- Graceful shutdown with in-flight spans
- No data loss under stress

## Performance Expectations

### Latency

- **Span submission** (hot path): <100ns (reserve + commit from LOW_LATENCY_CONFIG)
- **Consumer polling**: 100ms interval (10Hz) - configurable trade-off
- **Batch export**: 50-500ms depending on backend (network bound)
- **End-to-end**: <1s from span creation to backend visibility

### Throughput

Based on ringmpsc-rs benchmarks (9.21B msgs/sec peak):
- **Single producer**: ~1.5B spans/sec (limited by span size, not channel)
- **8 producers**: ~1B spans/sec aggregate
- **Realistic microservice**: 10K-100K spans/sec per service

### Memory

- **Ring buffers**: 16 producers × 4K spans × 88 bytes = 5.6 MB
- **Pending batches**: ~10K spans × 88 bytes = 880 KB
- **Total**: <10 MB for high-throughput collector

## Configuration Tuning

### Low Latency (Real-Time Applications)

```rust
CollectorConfig {
    ring_bits: 12,           // 4K spans, fits L1 cache
    max_producers: 16,
    batch_size: 1_000,       // Smaller batches, faster export
    batch_timeout: Duration::from_secs(1),
    consumer_interval: Duration::from_millis(50),  // 20Hz polling
}
```

### High Throughput (Batch Analytics)

```rust
CollectorConfig {
    ring_bits: 16,           // 64K spans, more buffering
    max_producers: 32,
    batch_size: 10_000,      // Larger batches, amortize export
    batch_timeout: Duration::from_secs(10),
    consumer_interval: Duration::from_millis(200), // 5Hz polling
}
```

## Dependencies

```toml
[package]
edition = "2024"  # Required for native async traits

[dependencies]
ringmpsc-rs = { path = "../.." }
tokio = { version = "1.35", features = ["full"] }
tonic = "0.10"                      # OTLP gRPC (future)
prost = "0.12"                      # Protobuf (future)
opentelemetry-proto = "0.5"         # OTLP spec (future)
reqwest = { version = "0.11", features = ["json"] }  # OTLP HTTP (future)
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
thiserror = "2.0"                   # Error handling
rand = "0.8"

[dev-dependencies]
criterion = "0.5"
```

**Note**: `async-trait` is no longer needed with Rust 2024 edition's native async traits.

## Future Enhancements

1. **Sampling strategies**: Head-based (pre-submission) or tail-based (post-collection)
2. **Compression**: gzip/zstd on export batches
3. **Multiple consumers**: Fan-out to multiple backends simultaneously
4. **Adaptive batching**: Dynamic batch size based on export latency
5. **Resource attributes**: Attach service metadata to all spans
6. **Span links**: Support trace context propagation across services

## Design Decisions

### Why Lock-Free MPSC Over Async Channels?

- **Latency**: <100ns submit vs 1-10µs for tokio::mpsc
- **Zero allocation**: Reserve/commit API avoids heap allocation per span
- **Batching**: consume_all_up_to() amortizes atomic overhead (Disruptor pattern)
- **Bounded**: Fixed ring size provides natural backpressure

### Why Separate Sync + Async Layers?

- **Hot path optimization**: Span submission stays lock-free, no async overhead
- **Flexibility**: Consumer can be async (I/O-bound export) or sync (CPU-bound processing)
- **Testability**: Sync core is easier to test deterministically

### Why OTLP Over Native Protocols?

- **Standardization**: Single implementation covers Jaeger, Zipkin, Honeycomb, Datadog
- **Efficiency**: Protobuf encoding is compact and fast
- **Future-proof**: OpenTelemetry is the industry standard (CNCF graduated project)

## File Structure

```
crates/span_collector/
├── DESIGN.md              # This file
├── README.md              # Quick start guide
├── spec.md                # Invariants specification
├── Cargo.toml             # Project manifest
├── src/
│   ├── lib.rs             # Re-exports
│   ├── span.rs            # Span data model
│   ├── collector.rs       # Sync SpanCollector
│   ├── async_bridge.rs    # AsyncSpanCollector
│   ├── batch_processor.rs # Batching logic
│   ├── exporter.rs        # SpanExporter trait + impls
│   ├── resilient_exporter.rs  # Retry, circuit breaker wrappers
│   └── rate_limiter.rs    # RateLimiter trait + impls
├── bin/
│   ├── demo.rs            # Demo application
│   └── span_generator.rs  # Multi-producer stress test
└── tests/
    └── integration.rs     # Integration tests
```

## References

- [OpenTelemetry Specification](https://opentelemetry.io/docs/specs/otel/)
- [OTLP Protocol](https://opentelemetry.io/docs/specs/otlp/)
- [LMAX Disruptor Pattern](https://lmax-exchange.github.io/disruptor/)
- [RingMPSC Zig Implementation](https://github.com/boonzy00/ringmpsc)
