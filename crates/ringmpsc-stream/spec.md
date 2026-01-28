# Async Stream Pipeline Specification

This document defines the invariants for the `ringmpsc-stream` crate, which provides async `Stream`/`Sink` adapters for ringmpsc channels.

**Rust 2024 Edition**: Uses `futures-core::Stream` and `futures-sink::Sink` traits with `pin_project_lite` for pinning.

## 1. Stream Invariants

### INV-STREAM-01: Per-Producer FIFO Ordering
```
∀ producer P: items from P are yielded in send order
```
Items from a single producer are received in the order they were sent. No global ordering across producers (inherits from ringmpsc).

**Implementation**: Delegates to `Channel::consume_all_owned()` which preserves per-ring FIFO.

### INV-STREAM-02: Hybrid Polling
```
wake_condition = data_notify.notified() ∨ poll_interval.tick()
```
The receiver uses event-driven wakeup (via `Notify`) with a configurable poll interval as a safety net for missed notifications.

**Rationale**: Pure event-driven may miss notifications due to races; pure polling adds latency. Hybrid provides best of both.

**Configuration**: `StreamConfig::poll_interval` (default: 10ms)

### INV-STREAM-03: Backpressure Relief Signaling
```
consume_count > 0 → backpressure_notify.notify_waiters()
```
After draining items, the receiver notifies waiting senders that space is available.

**Location**: [src/receiver.rs](src/receiver.rs)

### INV-STREAM-04: Graceful Shutdown Drain
```
shutdown() → drain_all_remaining → yield_all_buffered → return None
```
Shutdown drains all committed items before terminating the stream.

## 2. Sink Invariants

### INV-SINK-01: No Item Loss on Backpressure
```
try_send(item) returns Err(item) on full ring (item preserved)
send(item).await blocks until space available or closed
```
Items are never lost due to backpressure. Uses `reserve()`/`commit()` internally.

**Implementation**: [src/sender.rs](src/sender.rs) - uses `Producer::reserve()` which returns `None` when full, preserving the item.

### INV-SINK-02: Single Producer Per Ring
```
∀ RingSender: backed by exactly one Producer (one Ring)
RingSender: !Clone
```
Each `RingSender` owns a unique `Producer`. Cloning is not allowed to preserve the single-producer-per-ring invariant.

**Enforcement**: `RingSender` does not implement `Clone`. New senders via `SenderFactory::register()`.

### INV-SINK-03: Data Arrival Notification
```
successful_send → data_notify.notify_one()
```
After successfully sending an item, the sender notifies the receiver that data is available.

## 3. Channel Invariants

### INV-CH-01: Explicit Registration
```
SenderFactory::register() → Result<RingSender, StreamError>
SenderFactory: Clone  (can be shared across threads)
RingSender: !Clone    (one sender per ring)
```
Producers must explicitly register. The factory is cloneable, but each registration returns a unique sender.

**Rationale**: Matches ringmpsc semantics; avoids hidden `Arc<Mutex<Producer>>` overhead.

### INV-CH-02: Shared Notify Pair
```
(SenderFactory, RingReceiver) share:
  - data_notify: Arc<Notify>        // sender → receiver
  - backpressure_notify: Arc<Notify> // receiver → sender
  - shutdown_state: Arc<ShutdownState>
```
The sender factory and receiver share notification handles for bidirectional signaling.

### INV-CH-03: Closure Semantics
```
SenderFactory::close() → prevents new register() calls
                       → existing senders continue until ring full or dropped
RingReceiver::shutdown() → drains remaining items → returns None
```
Closure is two-phase: close prevents new registrations, shutdown drains existing items.

## 4. Shutdown Invariants

### INV-SHUT-01: Oneshot Signal
```
shutdown() → shutdown_tx.send(()) → consumer loop receives signal
```
Shutdown is triggered via oneshot channel, matching span_collector's pattern.

### INV-SHUT-02: Wake Blocked Senders
```
shutdown → backpressure_notify.notify_waiters()
```
During shutdown, blocked senders are woken so they can observe the closed state.

### INV-SHUT-03: Composable with take_until
```
// External cancellation via CancellationToken
let stream = receiver.take_until(token.cancelled());
```
The internal oneshot handles programmatic shutdown; external `take_until` enables hierarchical cancellation.

## 5. Memory Ordering

### INV-ORD-01: Notify Synchronization
```
sender: write_to_ring → notify_one()
         ↓ (synchronizes-with)
receiver: notified().await → read_from_ring
```
`Notify` provides the necessary synchronization for data visibility.

### INV-ORD-02: Shutdown State
```
shutdown_state uses Acquire/Release ordering:
  - is_closed(): Acquire
  - close(): Release
```
Ensures shutdown visibility across threads.

## 6. Configuration

| Parameter | Default | Description |
|-----------|---------|-------------|
| `poll_interval` | 10ms | Safety net poll frequency |
| `batch_hint` | 64 | Target items per drain cycle |

**Presets**:
- `StreamConfig::low_latency()`: 1ms poll, batch 16
- `StreamConfig::high_throughput()`: 50ms poll, batch 256

## 7. Error Handling

### INV-ERR-01: Error Types
```rust
pub enum StreamError {
    Full,                            // Ring buffer full (recoverable)
    Closed,                          // Channel closed (terminal)
    RegistrationFailed(ChannelError), // Too many producers
    ShutDown,                        // Stream shut down
}
```

### INV-ERR-02: Recoverability
```
is_recoverable(Full) = true
is_terminal(Closed | ShutDown) = true
```
`Full` errors can be retried; `Closed`/`ShutDown` are permanent.

---

## Verification

| Invariant | Test Coverage | debug_assert! Location |
|-----------|--------------|------------------------|
| INV-STREAM-01 | FIFO ordering tests | N/A (structural - ring buffer guarantees) |
| INV-STREAM-02 | Timer + notify interaction tests | N/A (behavioral - tokio::select!) |
| INV-STREAM-03 | Backpressure tests | `receiver.rs` → `debug_assert_backpressure_signaled!` |
| INV-STREAM-04 | Shutdown drain tests | `receiver.rs` → `debug_assert_shutdown_drained!` |
| INV-SINK-01 | try_send preservation tests | `sender.rs` → `debug_assert_item_preserved!` |
| INV-SINK-02 | Compile-time (no Clone impl) | N/A (compile-time via `!Clone`) |
| INV-SINK-03 | Integration tests | `sender.rs` → `debug_assert_data_notified!` |
| INV-CH-01 | Registration tests | `channel.rs` → `debug_assert_explicit_registration!` |
| INV-CH-02 | Structural | N/A (structural - shared Arc) |
| INV-CH-03 | Close/shutdown tests | N/A (structural - AtomicBool) |
| INV-SHUT-01 | Graceful shutdown tests | `shutdown.rs` → `debug_assert_shutdown_signaled!` |
| INV-SHUT-02 | Graceful shutdown tests | `shutdown.rs` → `debug_assert_senders_woken!` |

## debug_assert! Macros

All runtime invariant checks are in [src/invariants.rs](src/invariants.rs):

| Macro | Invariant | Purpose |
|-------|-----------|---------|
| `debug_assert_backpressure_signaled!` | INV-STREAM-03 | Verify backpressure_notify.notify_waiters() called after drain |
| `debug_assert_shutdown_drained!` | INV-STREAM-04 | Verify all items consumed before returning None |
| `debug_assert_item_preserved!` | INV-SINK-01 | Verify item returned on reserve failure |
| `debug_assert_data_notified!` | INV-SINK-03 | Verify data_notify.notify_one() called after send |
| `debug_assert_explicit_registration!` | INV-CH-01 | Document explicit registration via factory |
| `debug_assert_shutdown_signaled!` | INV-SHUT-01 | Verify shutdown oneshot sent |
| `debug_assert_senders_woken!` | INV-SHUT-02 | Verify blocked senders woken on shutdown |

## Related Specifications

- [Ring Buffer Specification](../ringmpsc/spec.md) - Underlying guarantees
- [Span Collector Specification](../span_collector/spec.md) - Similar async patterns
