# Span Collector Specification

This document defines the domain and implementation invariants for the OpenTelemetry span collector built on ringmpsc-rs.

**Rust 2024 Edition**: This crate uses native async traits. The `SpanExporter` and `RateLimiter` traits use `impl Future` return types for Send bounds, with companion `SpanExporterBoxed` and `RateLimiterBoxed` traits for object-safe dynamic dispatch.

## 1. Domain Invariants (OpenTelemetry Semantics)

### INV-DOM-01: Trace Identity
```
∀ span: span.trace_id is globally unique (128-bit)
```
A trace_id uniquely identifies a distributed trace across all services. Collision probability with random 128-bit IDs is negligible.

### INV-DOM-02: Span Identity
```
∀ span: (span.trace_id, span.span_id) is unique within collector lifetime
```
A span_id (64-bit) is unique within its trace.

### INV-DOM-03: Parent-Child Relationship
```
span.parent_span_id = 0  ⟺  span is root of trace
span.parent_span_id ≠ 0  →  ∃ parent: parent.span_id = span.parent_span_id
```
Root spans have no parent. Non-root spans reference an existing parent.

**Note**: Parent may arrive after child (out-of-order is valid in distributed tracing).

### INV-DOM-04: Time Ordering
```
span.start_time ≤ span.end_time
span.duration_nanos() = span.end_time - span.start_time ≥ 0
```
A span's start time precedes or equals its end time.

### INV-DOM-05: Status Finalization
A span should have `status ≠ Unset` before submission (i.e., `finish()` was called).

**Implementation**: [src/span.rs#L100-L107](../src/span.rs#L100-L107)

## 2. Data Structure Invariants

### INV-DS-01: Boxed Attributes
```rust
attributes: Box<HashMap<String, AttributeValue>>
```
Attributes are boxed to keep `Span` size predictable (~120 bytes without attributes). This enables efficient ring buffer storage.

**Rationale**: Unbounded inline `HashMap` would cause Span size to vary wildly, hurting cache efficiency.

### INV-DS-02: Owned String Fields
```rust
name: String  // not &str
```
Spans own their data. No lifetime parameters, enabling safe transfer through ring buffers.

### INV-DS-03: Span Size Awareness
The `Span` struct is designed for efficient ring buffer storage:
- Fixed-size fields at start (trace_id, span_id, times)
- Heap-indirection for variable-size data (attributes)

## 3. Ownership Transfer Invariants

### INV-OWN-01: Zero-Copy Path
```
Producer.submit(span) → Ring.push(span) → Consumer.consume_all_owned(|span|)
                      ↓                                    ↓
                 MaybeUninit::new()                assume_init_read()
```
Spans are moved (not cloned) through the entire pipeline:
1. Producer writes span into ring via `MaybeUninit::new()`
2. Consumer reads span via `assume_init_read()` (ownership transfer)
3. No cloning occurs at any point

### INV-OWN-02: Owned-Only Consumption
```rust
// CORRECT: owned consumption
collector.consume_all_up_to_owned(limit, |span: Span| { ... });

// NOT PROVIDED: reference consumption would force clone
// collector.consume_all_up_to(limit, |span: &Span| { ... });
```
The collector exposes only owned consumption APIs because `Span` contains heap allocations (`String`, `Box<HashMap>`). Reference-based consumption would force callers to clone.

**Location**: [src/collector.rs#L140-L152](../src/collector.rs#L140-L152)

## 4. Batch Processing Invariants

### INV-BATCH-01: Trace Grouping
```
∀ batch: spans in batch are grouped by trace_id before export
```
Spans are accumulated in `HashMap<u128, Vec<Span>>` keyed by trace_id.

**Implementation**: [src/batch_processor.rs#L42](../src/batch_processor.rs#L42)

### INV-BATCH-02: Flush Triggers
```
should_flush() ⟺ (total_pending ≥ batch_size_limit) ∨ (elapsed ≥ batch_timeout)
```
Batches are flushed when either:
1. Size limit reached (default: 10,000 spans)
2. Timeout elapsed (default: 5 seconds)

### INV-BATCH-03: Drain on Shutdown
On shutdown, all remaining spans are consumed and exported:
```rust
// In shutdown handler
collector.consume_all(|span| batch_processor.add(span));
if let Some(batch) = batch_processor.take_batch() {
    exporter.export_boxed(batch).await;  // Final flush is sequential
}
// Wait for in-flight exports to complete
while let Some(result) = export_tasks.join_next().await { ... }
```

**Location**: [src/async_bridge.rs](src/async_bridge.rs)

### INV-BATCH-04: Concurrent Export Bound
```
inflight_exports ≤ max_concurrent_exports  (always)
```
The number of concurrent export tasks is bounded by a `Semaphore`. This prevents unbounded memory growth when export latency exceeds span arrival rate.

**Implementation**: [src/async_bridge.rs](src/async_bridge.rs) - `export_semaphore`

### INV-BATCH-05: Separation of Concerns
```rust
// BatchProcessor: pure batching (no atomics, no Arc)
BatchMetrics { spans_exported: u64, ... }  // Plain counters for sequential use

// ExportMetrics: concurrent tracking (in async_bridge)
ExportMetrics { spans_exported: AtomicU64, ... }  // Atomics for concurrent tasks
```
`BatchProcessor` is a pure batching abstraction with no concurrency overhead.
`ExportMetrics` in async_bridge handles concurrent export tracking with atomics.

**Implementation**: 
- [src/batch_processor.rs](src/batch_processor.rs) - `BatchMetrics` (plain u64)
- [src/async_bridge.rs](src/async_bridge.rs) - `ExportMetrics` (AtomicU64)
`BatchMetrics` uses `AtomicU64` for all counters because concurrent export tasks update metrics simultaneously.

**Implementation**: [src/batch_processor.rs](src/batch_processor.rs) - `BatchMetrics`

## 5. Backpressure Invariants

### INV-BP-01: Notify on Drain
```
consumer.consume() > 0  →  notify.notify_waiters()
```
When the consumer drains spans, it signals waiting producers that space is available.

**Implementation**: [src/async_bridge.rs#L81-L83](../src/async_bridge.rs#L81-L83)

### INV-BP-02: Bounded Retry
```rust
// Producer with backpressure
match producer.try_submit(span) {
    Err(SubmitError::Full) => {
        notify.notified().await;  // Wait for space
        // Retry...
    }
}
```
Producers don't spin when ring is full; they await notification.

### INV-BP-03: Latency Bound
```
consumer_interval (default 100ms) bounds worst-case latency for backpressure relief
```
Even if no spans are consumed (unlikely), producers wait at most one interval before retry.

## 6. Metrics Invariants

### INV-MET-01: Submission Accounting
```
metrics.spans_submitted = Σ successful submit() calls
metrics.spans_consumed = Σ spans passed to consume handler
```

### INV-MET-02: Conservation (Eventually)
```
spans_submitted ≥ spans_consumed  (always)
spans_submitted = spans_consumed  (when drained)
```
No span loss: every submitted span is eventually consumed (assuming consumer runs).

### INV-MET-03: Relaxed Ordering is Safe
```rust
spans_submitted.fetch_add(1, Ordering::Relaxed)  // OK
```
Metrics use `Relaxed` ordering because:
1. No control flow depends on metric values
2. No happens-before relationships needed
3. Eventual consistency is acceptable for observability

**Location**: [src/collector.rs#L52-L60](../src/collector.rs#L52-L60)

## 7. Async Bridge Invariants

### INV-ASYNC-01: Consumer Task Lifecycle
```
new() → spawns consumer task
shutdown() → signals task, awaits completion
drop() → consumer task stops (may not fully drain)
```

### INV-ASYNC-02: Graceful Shutdown Protocol
```rust
shutdown_tx.send(())          // 1. Signal shutdown
consumer_task.await           // 2. Wait for drain + final flush
```

### INV-ASYNC-03: Tokio Integration
The bridge uses `tokio::sync::Notify` (not `std::sync::Condvar`) for async-aware signaling.

## 8. Resilience Invariants

### INV-RES-01: Retry Attempt Bound
```
attempts ≤ max_retries + 1
```
The total number of export attempts is bounded by configuration. After exhausting retries, `ExportError::RetriesExhausted` is returned.

**Implementation**: [src/resilient_exporter.rs](src/resilient_exporter.rs) - `RetryingExporter::export()`

### INV-RES-02: Exponential Backoff Delay
```
delay(attempt) = min(initial_delay × multiplier^(attempt-1), max_delay)
```
Retry delays grow exponentially but are capped at `max_delay` to prevent unbounded waits.

### INV-RES-03: Circuit Breaker State Machine
```
Closed → Open:    consecutive_failures ≥ failure_threshold
Open → HalfOpen:  elapsed ≥ reset_timeout
HalfOpen → Closed: consecutive_successes ≥ success_threshold
HalfOpen → Open:   any failure
```
Circuit breaker transitions follow a deterministic state machine.

**Implementation**: [src/resilient_exporter.rs](src/resilient_exporter.rs) - `CircuitBreakerExporter`

### INV-RES-04: Circuit Open Fail-Fast
```
state = Open ∧ elapsed < reset_timeout → return Err(CircuitOpen) immediately
```
When the circuit is open, export attempts fail immediately without calling the inner exporter.

### INV-RES-05: Rate Limiter Pacing
```
∀ consecutive calls: time(call_n+1) - time(call_n) ≥ interval
```
Rate-limited operations are paced according to the configured interval.

**Implementation**: [src/rate_limiter.rs](src/rate_limiter.rs) - `IntervalRateLimiter`

### INV-RES-06: Async Mutex for Rate Limiter
```rust
// CORRECT: tokio::sync::Mutex holds across await
let mut limiter = self.rate_limiter.lock().await;
limiter.wait().await;
// lock released here

// WRONG: std::sync::Mutex cannot be held across await
```
`RateLimitedExporter` uses `tokio::sync::Mutex` because the rate limiter must be held across the async `wait()` call.

**Implementation**: [src/resilient_exporter.rs](src/resilient_exporter.rs) - `RateLimitedExporter`

## 9. Object Safety Invariants

### INV-OBJ-01: Boxed Trait Pattern
```rust
// SpanExporter: not object-safe (impl Future return type)
trait SpanExporter {
    fn export(&self, batch: SpanBatch) -> impl Future<...> + Send;
}

// SpanExporterBoxed: object-safe wrapper
trait SpanExporterBoxed {
    fn export_boxed(&self, batch: SpanBatch) -> Pin<Box<dyn Future<...> + Send + '_>>;
}

// Blanket impl enables: Arc<dyn SpanExporterBoxed>
impl<T: SpanExporter> SpanExporterBoxed for T { ... }
```
Native async traits with `impl Future` are not object-safe. The `*Boxed` traits provide object safety via `Pin<Box<dyn Future>>`.

**Implementation**: [src/exporter.rs](src/exporter.rs), [src/rate_limiter.rs](src/rate_limiter.rs)

### INV-OBJ-02: Dynamic Dispatch Consumers
```rust
// BatchProcessor and AsyncSpanCollector use SpanExporterBoxed
exporter: Arc<dyn SpanExporterBoxed>

// Calls use export_boxed() method
self.exporter.export_boxed(batch).await
```
Components that need dynamic exporter dispatch use `SpanExporterBoxed`.

## 10. Design Principles

### PRIN-01: Trait Bounds on Domain-Specific Types

For domain-specific wrapper types where the bound is fundamental to the type's identity, the bound belongs on the struct definition itself:

```rust
// ✅ CORRECT: Bound on struct - this type IS an exporter wrapper
pub struct RetryingExporter<E: SpanExporter> { ... }
pub struct CircuitBreakerExporter<E: SpanExporter> { ... }
pub struct RateLimitedExporter<E: SpanExporter, R: RateLimiter> { ... }

// ❌ INCORRECT for this use case: Bound only on impl
pub struct RetryingExporter<E> { ... }  // Allows RetryingExporter<NotAnExporter>
```

**Rationale**: The guideline "don't put bounds on structs" applies to general-purpose containers like `Vec<T>` or `HashMap<K, V>`. For types where:
1. The type name includes the trait concept ("Exporter" in `RetryingExporter`)
2. The type only makes sense when wrapping that trait
3. Using the type with a non-implementing type is always a bug

...the bound should be on the struct to document intent and prevent misuse at the type level.

**Applied to**: `RetryingExporter`, `CircuitBreakerExporter`, `RateLimitedExporter`, `ResilientExporterBuilder`

---

## Verification

| Invariant | Verification Method |
|-----------|-------------------|
| INV-DOM-* | Unit tests in [tests/](tests/) |
| INV-DS-* | Compile-time (Rust type system) |
| INV-OWN-* | Compile-time (Rust ownership) + Miri |
| INV-BATCH-01 | Integration tests |
| INV-BATCH-02 | Integration tests |
| INV-BATCH-03 | `test_graceful_shutdown_with_inflight_spans` |
| INV-BATCH-04 | Structural (Semaphore bounds concurrent tasks) |
| INV-BATCH-05 | Structural (separate types: `BatchMetrics` vs `ExportMetrics`) |
| INV-BP-* | Load tests with slow consumer |
| INV-MET-* | Stress tests comparing counts |
| INV-ASYNC-* | Tokio test runtime |
| INV-RES-01 | `test_retry_exhausted` |
| INV-RES-02 | `test_retry_succeeds_after_failures` |
| INV-RES-03 | `test_circuit_breaker_opens_on_failures`, `test_circuit_breaker_half_open_recovery` |
| INV-RES-04 | `test_circuit_breaker_opens_on_failures` (verifies CircuitOpen error) |
| INV-RES-05 | `test_rate_limited_exporter` |
| INV-RES-06 | Compile-time (async Mutex type) |
| INV-OBJ-* | Compile-time (trait object rules) |

## debug_assert! Coverage

| Invariant | Location | Assertion |
|-----------|----------|-----------|
| INV-RES-01 | `resilient_exporter.rs` | `debug_assert!(attempt < max_attempts)` |
| INV-RES-03 | `resilient_exporter.rs` | `debug_assert_circuit_state_valid!` |

## Related Specifications

- [Ring Buffer Specification](../ringmpsc/spec.md) - Low-level guarantees this collector depends on
- [Backpressure Pattern](docs/backpressure-notify-pattern.md) - Detailed design doc