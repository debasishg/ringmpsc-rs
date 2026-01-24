# Span Collector Specification

This document defines the domain and implementation invariants for the OpenTelemetry span collector built on ringmpsc-rs.

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
batch_processor.flush().await;
```

**Location**: [src/async_bridge.rs#L91-L99](../src/async_bridge.rs#L91-L99)

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

---

## Verification

| Invariant | Verification Method |
|-----------|-------------------|
| INV-DOM-* | Unit tests in [tests/](../tests/) |
| INV-OWN-* | Compile-time (Rust ownership) + Miri |
| INV-BATCH-* | Integration tests |
| INV-BP-* | Load tests with slow consumer |
| INV-MET-* | Stress tests comparing counts |
| INV-ASYNC-* | Tokio test runtime |

## Related Specifications

- [Ring Buffer Specification](../ringmpsc/spec.md) - Low-level guarantees this collector depends on
- [Backpressure Pattern](docs/backpressure-notify-pattern.md) - Detailed design doc
