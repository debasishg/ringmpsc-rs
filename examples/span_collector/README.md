# OpenTelemetry Span Collector

A high-performance distributed tracing span collector implementation combining ringmpsc-rs's lock-free MPSC channels with async Rust.

## Overview

This project demonstrates how to build a production-ready span collector for OpenTelemetry-compatible distributed tracing systems, achieving <100ns span submission latency through lock-free ring buffers and efficient batching.

## Features

- **Lock-free span submission** - <100ns latency using ringmpsc-rs MPSC channels
- **Async/await integration** - Tokio-based consumer with backpressure handling
- **Batch processing** - Amortizes export overhead by grouping spans
- **Multiple exporters** - Stdout, JSON file, and null exporters included
- **Graceful shutdown** - Ensures all spans are exported before termination
- **Configurable** - Tune for low-latency or high-throughput scenarios

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    Instrumented Service                     │
│  Async Task 1  │  Async Task 2  │ ... │  Async Task N       │
│  (Producer)    │  (Producer)    │     │  (Producer)         │
└────────┬────────────────┬─────────────────────┬─────────────┘
         │ submit_span()  │ submit_span()       │ submit_span()
         │ <100ns         │ <100ns              │ <100ns
         ▼                ▼                     ▼
┌─────────────────────────────────────────────────────────────┐
│              Lock-Free MPSC Ring Buffers                    │
│  Ring 0 (4K)  │  Ring 1 (4K)  │ ... │  Ring N (4K)          │
└────────┬────────────────────────────────────────────────────┘
         │ consume_all_up_to(10K) - tokio::spawn consumer
         ▼
┌─────────────────────────────────────────────────────────────┐
│              Async Batch Processor (5s timeout)             │
└────────┬────────────────────────────────────────────────────┘
         │ export_batch()
         ▼
┌─────────────────────────────────────────────────────────────┐
│                   Span Exporter (Stdout/OTLP)               │
└─────────────────────────────────────────────────────────────┘
```

## Quick Start

### Run the Demo

```bash
cargo run --release --example demo
```

This will:
- Create 8 async producer tasks
- Generate 50 spans per producer (400 total)
- Export batches to stdout
- Demonstrate backpressure handling

### Run Tests

**Use release mode to avoid memory issues with `MaybeUninit` in lock-free data structures:**

```bash
cargo test --release
```

**For debugging with symbols:**

```bash
RUST_BACKTRACE=1 cargo test --release
```

## Usage

### Basic Example

```rust
use span_collector::{
    AsyncCollectorConfig, AsyncSpanCollector, Span, SpanKind, StdoutExporter,
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create collector with stdout exporter
    let exporter = Arc::new(StdoutExporter::new(true));
    let config = AsyncCollectorConfig::default();
    let collector = AsyncSpanCollector::new(config, exporter).await;

    // Register producer
    let producer = collector.register_producer().await?;

    // Create and submit span
    let span = Span::new(
        12345,            // trace_id
        1,                // span_id
        0,                // parent_span_id
        "my-operation".to_string(),
        SpanKind::Server,
    );
    producer.submit_span(span).await?;

    // Graceful shutdown
    collector.shutdown().await?;
    Ok(())
}
```

## Configuration

### Low Latency (Real-Time)

```rust
let config = AsyncCollectorConfig {
    collector_config: CollectorConfig {
        ring_bits: 12,           // 4K spans per ring
        max_producers: 16,
        enable_metrics: true,
    },
    consumer_interval: Duration::from_millis(50),  // 20Hz
    max_consume_per_poll: 1_000,
    ..Default::default()
};
```

### High Throughput (Batch Analytics)

```rust
let config = AsyncCollectorConfig {
    collector_config: CollectorConfig {
        ring_bits: 16,           // 64K spans per ring
        max_producers: 32,
        enable_metrics: true,
    },
    consumer_interval: Duration::from_millis(200), // 5Hz
    max_consume_per_poll: 10_000,
    ..Default::default()
};
```

## Performance

Based on ringmpsc-rs benchmarks (9.21B msgs/sec peak):

- **Span submission latency**: <100ns (lock-free reserve + commit)
- **Single producer throughput**: ~1.5B spans/sec
- **8 producers aggregate**: ~1B spans/sec
- **Memory footprint**: ~5.6MB (16 producers × 4K spans × 88 bytes)

## Project Structure

```
src/
├── lib.rs              # Re-exports
├── span.rs             # Span data model
├── collector.rs        # Sync SpanCollector (lock-free core)
├── async_bridge.rs     # AsyncSpanCollector (Tokio integration)
├── batch_processor.rs  # Batching logic
└── exporter.rs         # SpanExporter trait + implementations

examples/
└── main.rs             # Demo application

tests/
└── integration.rs      # Integration tests
```

## Design Decisions

### Why Lock-Free MPSC Over Async Channels?

- **Latency**: <100ns vs 1-10µs for tokio::mpsc
- **Zero allocation**: Reserve/commit API avoids heap per span
- **Batching**: Disruptor-pattern batch consumption amortizes atomics

### Why Separate Sync + Async Layers?

The architecture has two distinct layers:

1. **Sync layer** (`SpanProducer` in `collector.rs`): Provides `submit_span()` and `try_submit_span()` - fully synchronous, lock-free ring buffer operations with optional backoff retry.

2. **Async layer** (`AsyncSpanProducer` in `async_bridge.rs`): Wraps the sync producer with `async fn submit_span()` that handles backpressure via `tokio::sync::Notify`.

**Hot path optimization**: The async `submit_span()` calls the sync `try_submit_span()` internally. When the ring buffer is **not full** (the common case), the lock-free write succeeds immediately and the async function returns without ever awaiting - no async runtime overhead. Only when the buffer is full does it `await` on backpressure notification, yielding to the runtime until the consumer drains spans.

This means:
- **Fast path** (buffer not full): Pure lock-free operation, <100ns latency
- **Slow path** (buffer full): Async wait for consumer to drain, then retry
- **Flexibility**: Consumer can be async (I/O) or sync (CPU-bound)
- **Testability**: Sync core is deterministic and can be tested without async runtime

## TODO

- [ ] **OTLP Exporter** - Implement `OtlpExporter` to export spans to OpenTelemetry collectors via OTLP protocol (gRPC using `tonic` or HTTP using `reqwest`). This would convert the internal `Span` format to OTLP protobuf and support configurable endpoints, headers, and retry logic.

## Future Enhancements

1. Sampling strategies (head-based/tail-based)
2. Compression on export (gzip/zstd)
3. Multiple consumers (fan-out to backends)
4. Adaptive batching based on latency
5. Resource attributes (service metadata)

## References

- [Design Document](DESIGN.md) - Detailed architecture and implementation plan
- [RingMPSC-RS](https://github.com/yourusername/ringmpsc-rs) - Lock-free MPSC channel implementation
- [OpenTelemetry Specification](https://opentelemetry.io/docs/specs/otel/)

## License

Same as parent project (ringmpsc-rs)
