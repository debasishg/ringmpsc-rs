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
The receiver uses event-driven wakeup (via `Notify`) with a configurable poll interval as a safety net for batching and edge cases.

**Rationale**: The register-then-recheck pattern (INV-STREAM-05) closes the in-flight race window (items arriving during `poll_next` execution). The timer is required for waking the task when items arrive between `poll_next` calls, since the `Notified` future is stack-local and dropped on return. The timer also enables batching.

**Configuration**: `StreamConfig::poll_interval` (default: 10ms)

### INV-STREAM-05: Register-Then-Recheck (Notification Race Fix)
```
data_notified.poll(cx) → Pending  →  re-drain ring buffer
```
After registering the waker via `data_notified.poll(cx)`, the receiver
immediately re-drains the ring buffer. This closes the in-flight race
window where a producer pushes an item and calls `notify_one()` between
the last drain and the waker registration.

**Scope**: This fix covers items arriving *during* `poll_next` execution.
Items arriving *after* `poll_next` returns `Pending` are handled by
the timer (INV-STREAM-02), since the `Notified` future is dropped on
return and `notify_one()` can only store a permit, not wake the task.

**Race sequence (without this fix)**:
1. Consumer drains ring → empty
2. Consumer calls `data_notify.notified()` → creates `Notified` future
3. Producer pushes item → calls `notify_one()` → wakes task
4. Consumer polls `data_notified` → `Pending` (notification consumed by wake)
5. Consumer returns `Poll::Pending` → `data_notified` dropped
6. On re-entry: `data_pending` is false → skips drain → items stuck until timer

**Fix**: Step 4½ — after `Pending`, drain the ring. Any items found were
pushed in the race window.

**Location**: [src/receiver.rs](src/receiver.rs) — register-then-recheck block

**Assertion**: `debug_assert_recheck_after_register!` in [src/invariants.rs](src/invariants.rs)

### INV-STREAM-06: Timer Tick Loop (Waker Registration Guarantee)
```
loop { poll_tick(cx) → Ready: drain, continue; Pending: break }
```
`tokio::time::Interval::poll_tick()` only registers the waker when it returns
`Poll::Pending`. If called once and it returns `Ready` (first tick fires
immediately on construction), the waker is never registered and the timer
permanently stops waking the task.

**Bug (without this fix)**:
1. `Interval` created → first tick is immediately `Ready`
2. `poll_tick(cx)` returns `Ready` → drains ring, returns items
3. On next `poll_next` call, `poll_tick(cx)` returns `Ready` again (elapsed)
4. Eventually no ticks are elapsed → `poll_tick` returns `Pending` (registers waker)
5. But if `poll_next` only called `poll_tick` once per invocation, step 4 never happens
6. Timer is dead → receiver relies solely on `Notify` (which drops on return) → stall

**Fix**: Loop over `poll_tick` calls, consuming all elapsed `Ready` ticks,
until `Pending` is returned. The `Pending` return registers the waker for the
next tick, keeping the timer alive.

**Location**: [src/receiver.rs](src/receiver.rs) — timer polling loop

**Assertion**: `debug_assert_timer_pending_reached!` (structural — placed at Pending branch)

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
| INV-STREAM-05 | Lost-wakeup regression tests (timer disabled) | `receiver.rs` → `debug_assert_recheck_after_register!` |
| INV-STREAM-06 | Burst pattern tests (verify timer stays alive) | `receiver.rs` → `debug_assert_timer_pending_reached!` (structural) |
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
| `debug_assert_recheck_after_register!` | INV-STREAM-05 | Verify post-registration re-drain caught items in race window |
| `debug_assert_timer_pending_reached!` | INV-STREAM-06 | Structural: placed at Pending branch of timer loop to document waker registration |
| `debug_assert_item_preserved!` | INV-SINK-01 | Verify item returned on reserve failure |
| `debug_assert_data_notified!` | INV-SINK-03 | Verify data_notify.notify_one() called after send |
| `debug_assert_explicit_registration!` | INV-CH-01 | Document explicit registration via factory |
| `debug_assert_shutdown_signaled!` | INV-SHUT-01 | Verify shutdown oneshot sent |
| `debug_assert_senders_woken!` | INV-SHUT-02 | Verify blocked senders woken on shutdown |

## Related Specifications

- [Ring Buffer Specification](../ringmpsc/spec.md) - Underlying guarantees
- [Span Collector Specification](../span_collector/spec.md) - Similar async patterns
