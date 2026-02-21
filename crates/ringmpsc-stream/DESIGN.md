# Async Stream Pipeline Design

## Overview

`ringmpsc-stream` provides async `futures::Stream` and `futures::Sink` adapters for ringmpsc-rs channels. It enables building high-throughput async pipelines with cooperative backpressure, combining the lock-free performance of ringmpsc with Rust's async ecosystem.

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                        Async Application Layer                          │
├─────────────────────────────────────────────────────────────────────────┤
│   Task 1         Task 2         Task 3             Task N               │
│   ┌──────┐       ┌──────┐       ┌──────┐           ┌──────┐             │
│   │Sender│       │Sender│       │Sender│    ...    │Sender│             │
│   └──┬───┘       └──┬───┘       └──┬───┘           └──┬───┘             │
│      │              │              │                  │                 │
│      │ send()       │ send()       │ send()          │ send()           │
│      │ (await on    │ (await on    │ (await on       │ (await on        │
│      │  backpressure)│  backpressure)│  backpressure) │  backpressure)  │
└──────┼──────────────┼──────────────┼──────────────────┼─────────────────┘
       │              │              │                  │
       ▼              ▼              ▼                  ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                     Lock-Free MPSC Ring Buffers                         │
│  ┌─────────────┐ ┌─────────────┐ ┌─────────────┐   ┌─────────────┐      │
│  │   Ring 0    │ │   Ring 1    │ │   Ring 2    │...│   Ring N    │      │
│  │ [Item|Item] │ │ [Item|Item] │ │ [Item|Item] │   │ [Item|Item] │      │
│  └─────────────┘ └─────────────┘ └─────────────┘   └─────────────┘      │
│                                                                         │
│  Each RingSender owns exactly one ring (INV-SINK-02)                    │
└────────────────────────────────┬────────────────────────────────────────┘
                                 │
                                 │ consume_all_owned()
                                 │ (hybrid polling)
                                 ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                          RingReceiver                                   │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │  poll_next()                                                    │    │
│  │  ┌─────────────────────────────────────────────────────────────┐│    │
│  │  │ tokio::select! {                                            ││    │
│  │  │     _ = data_notify.notified() => { drain(); yield; }       ││    │
│  │  │     _ = poll_timer.tick() => { drain(); yield; }  (safety)  ││    │
│  │  │     _ = shutdown_rx => { final_drain(); return None; }      ││    │
│  │  │ }                                                           ││    │
│  │  └─────────────────────────────────────────────────────────────┘│    │
│  └─────────────────────────────────────────────────────────────────┘    │
└────────────────────────────────┬────────────────────────────────────────┘
                                 │
                                 │ Stream<Item=T>
                                 ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                     Downstream Processing                               │
│  .map(transform) → .filter(predicate) → .chunks(N) → collect/process    │
└─────────────────────────────────────────────────────────────────────────┘
```

## Key Design Decisions

### 1. Hybrid Polling Strategy (INV-STREAM-02)

**Problem**: Pure event-driven polling via `Notify` can miss notifications due to races. Pure interval polling adds latency.

**Solution**: Combine both approaches:

```
┌─────────────────────────────────────────────┐
│           Hybrid Polling Strategy           │
├─────────────────────────────────────────────┤
│                                             │
│   Event-Driven Path (fast)                  │
│   ┌─────────────────────────────────────┐   │
│   │ Sender: data_notify.notify_one()    │   │
│   │           │                         │   │
│   │           ▼                         │   │
│   │ Receiver: data_notify.notified()    │   │
│   │           wakes immediately         │   │
│   └─────────────────────────────────────┘   │
│                                             │
│   + Safety Net (catch missed notifies)      │
│   ┌─────────────────────────────────────┐   │
│   │ poll_timer.tick() every 10ms        │   │
│   │ Ensures wake within poll_interval   │   │
│   └─────────────────────────────────────┘   │
│                                             │
└─────────────────────────────────────────────┘
```

**Configuration**: `StreamConfig::poll_interval` (default 10ms)

### 2. Explicit Producer Registration (INV-CH-01)

**Problem**: Should senders be `Clone`? How to handle multi-producer?

**Decision**: Explicit registration, no `Clone`:

```rust
// SenderFactory is Clone (can share across threads)
let (factory, rx) = channel::<Item>(config);

// Each sender must register explicitly
let tx1 = factory.register()?;  // Gets Ring 0
let tx2 = factory.register()?;  // Gets Ring 1

// RingSender is NOT Clone - one sender per ring
// let tx3 = tx1.clone();  // Compile error!
```

**Rationale**:
- Matches ringmpsc's single-producer-per-ring invariant
- Avoids hidden `Arc<Mutex<Producer>>` overhead
- Clear ownership semantics
- Each sender's backpressure is independent

### 3. Zero-Copy Send via Reserve/Commit (INV-SINK-01)

**Problem**: ringmpsc's `push()` consumes the item by value. If the ring is full, the item is lost.

**Solution**: Use `reserve()`/`commit()` API to preserve items on backpressure:

```
┌─────────────────────────────────────────────────────────────────┐
│                    Send with Backpressure                       │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│   1. Try reserve(1)                                             │
│      ┌────────────────────────────────────────────────────────┐ │
│      │ if let Some(reservation) = producer.reserve(1) {       │ │
│      │     reservation.as_mut_slice()[0] = MaybeUninit::new();│ │
│      │     reservation.commit();  // Item now in ring         │ │
│      │ }                                                      │ │
│      └────────────────────────────────────────────────────────┘ │
│                                                                 │
│   2. If reserve returns None (ring full):                       │
│      ┌────────────────────────────────────────────────────────┐ │
│      │ // Item is STILL OWNED by caller (not consumed!)       │ │
│      │ backpressure_notify.notified().await;  // Wait         │ │
│      │ // Retry with same item                                │ │
│      └────────────────────────────────────────────────────────┘ │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### 4. Graceful Shutdown (INV-SHUT-01, INV-SHUT-02)

**Pattern**: Two-phase shutdown matching span_collector's design:

```
Phase 1: Close for new registrations
┌─────────────────────────────────────────┐
│ factory.close()                         │
│   → shutdown_state.close()              │
│   → channel.close()                     │
│   → New register() calls return Closed  │
│   → Existing senders continue working   │
└─────────────────────────────────────────┘

Phase 2: Drain and terminate
┌─────────────────────────────────────────┐
│ receiver.shutdown()                     │
│   → shutdown_tx.send(())                │
│   → Consumer loop receives signal       │
│   → consume_all_owned() - final drain   │
│   → notify_waiters() - wake senders     │
│   → Stream returns None                 │
└─────────────────────────────────────────┘
```

**Composability**: Internal oneshot handles programmatic shutdown. External cancellation via `StreamExt::take_until`:

```rust
// Programmatic shutdown
rx.shutdown();

// OR external cancellation token
let token = CancellationToken::new();
let stream = rx.take_until(token.cancelled());
```

### 5. Backpressure Signaling

**Dual Notify Pattern**:

```
┌─────────────┐                    ┌─────────────┐
│  RingSender │                    │RingReceiver │
├─────────────┤                    ├─────────────┤
│             │  data_notify       │             │
│  send() ────┼───────────────────▶│ poll_next() │
│             │  .notify_one()     │ wakes       │
│             │                    │             │
│             │  backpressure_     │             │
│  awaits ◀───┼─────────────────── │ drain()     │
│             │  .notify_waiters() │ signals     │
└─────────────┘                    └─────────────┘
```

- **data_notify**: Sender → Receiver (item available)
- **backpressure_notify**: Receiver → Sender (space available)

**Shared Ownership via Arc**:

Both `RingSender` and `RingReceiver` hold `Arc<Notify>` pointing to the **same** heap-allocated `Notify` instance. The `Notify` is created once during channel construction in `channel.rs`:

```rust
let backpressure_notify = Arc::new(Notify::new());  // ONE instance created

// Receiver gets a clone of the Arc pointer (not a new Notify)
let receiver = RingReceiver::new(
    Arc::clone(&backpressure_notify),  // Points to same Notify
    ...
);

// Each sender also gets a clone of the Arc pointer
Ok(RingSender::new(
    Arc::clone(&self.backpressure_notify),  // Points to same Notify
    ...
))
```

**Thread-Safe Coordination Flow**:

```
┌─────────────────────────────────────────────────────────────────────────┐
│  Step 1: SENDER - Ring Full, Wait for Space                             │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│   producer.reserve(1) returns None (ring is full)                       │
│                     │                                                   │
│                     ▼                                                   │
│   backpressure_notify.notified().await                                  │
│     │                                                                   │
│     ├─► Atomically registers this task as a waiter inside the Notify    │
│     └─► Suspends execution (returns Poll::Pending to executor)          │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
                                    │
                                    │ (task suspended, waiting)
                                    ▼
┌─────────────────────────────────────────────────────────────────────────┐
│  Step 2: RECEIVER - Drain Items, Signal Space Available                 │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│   channel.consume_all_up_to_owned(batch_limit, |item| { ... });         │
│                     │                                                   │
│                     ▼                                                   │
│   backpressure_notify.notify_waiters()                                  │
│     │                                                                   │
│     ├─► Atomically grants permits to ALL currently registered waiters   │
│     └─► Wakes their wakers so the executor re-polls them                │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
                                    │
                                    │ (permit granted, waker invoked)
                                    ▼
┌─────────────────────────────────────────────────────────────────────────┐
│  Step 3: SENDER - Resume and Retry                                      │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│   notified().await completes (Poll::Ready)                              │
│                     │                                                   │
│                     ▼                                                   │
│   Loop continues: retry producer.reserve(1)                             │
│   (Space should now be available since receiver drained items)          │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

**Why `notify_waiters()` instead of `notify_one()`?**

The receiver uses `notify_waiters()` because multiple senders (each with their own ring) might be blocked waiting for backpressure relief. Using `notify_waiters()` wakes **all** currently waiting tasks, ensuring no sender remains unnecessarily blocked after space becomes available.

**Lock-Free Coordination**:

The `Notify` internally uses atomic operations (no mutex) to manage the waiter list. This makes the sender-receiver coordination lock-free and safe across threads without serialization overhead.

## Components

### 1. SenderFactory (`channel.rs`)

```rust
pub struct SenderFactory<T> {
    channel: Arc<Channel<T>>,
    data_notify: Arc<Notify>,
    backpressure_notify: Arc<Notify>,
    shutdown_state: Arc<ShutdownState>,
}

impl<T> Clone for SenderFactory<T> { ... }  // Shareable

impl<T> SenderFactory<T> {
    pub fn register(&self) -> Result<RingSender<T>, StreamError>;
    pub fn close(&self);
}
```

### 2. RingSender (`sender.rs`)

```rust
pub struct RingSender<T> {
    producer: Producer<T>,
    data_notify: Arc<Notify>,
    backpressure_notify: Arc<Notify>,
    shutdown_state: Arc<ShutdownState>,
    pending_item: Option<T>,  // For Sink::start_send buffering
}

// NOT Clone!
impl<T> Sink<T> for RingSender<T> { ... }
```

### 3. RingReceiver (`receiver.rs`)

```rust
pub struct RingReceiver<T> {
    channel: Arc<Channel<T>>,
    data_notify: Arc<Notify>,
    backpressure_notify: Arc<Notify>,
    shutdown_state: Arc<ShutdownState>,
    shutdown_rx: Option<oneshot::Receiver<()>>,
    config: StreamConfig,
    poll_timer: Interval,
    buffer: VecDeque<T>,  // Batch buffer
}

impl<T> Stream for RingReceiver<T> {
    type Item = T;
    fn poll_next(...) -> Poll<Option<T>>;
}
```

### 4. StreamConfig (`config.rs`)

| Parameter | Default | Description |
|-----------|---------|-------------|
| `poll_interval` | 10ms | Hybrid polling safety net interval |
| `batch_hint` | 64 | Target items per drain cycle |

**Presets**:
- `StreamConfig::low_latency()`: 1ms poll, batch 16
- `StreamConfig::high_throughput()`: 50ms poll, batch 256

## Error Handling

```rust
pub enum StreamError {
    Full,                            // Ring full (recoverable)
    Closed,                          // Channel closed (terminal)
    RegistrationFailed(ChannelError), // Too many producers
    ShutDown,                        // Receiver shut down
}
```

**Recoverability**:
- `Full`: Retry after backpressure relief
- `Closed`, `ShutDown`, `RegistrationFailed`: Terminal, propagate error

## Performance Considerations

### Memory Layout

```
RingSender<T>:  ~72 bytes (3 Arc pointers + Producer + Option<T>)
RingReceiver<T>: ~200 bytes (Channel Arc + 2 Notify Arcs + VecDeque + Interval)
```

### Throughput Optimizations

1. **Batch draining**: `consume_all_up_to_owned(batch_hint)` reduces atomic ops
2. **Notify batching**: `notify_waiters()` wakes all blocked senders at once
3. **Zero-copy path**: `reserve()`/`commit()` avoids cloning
4. **Polling hybrid**: Event-driven for low latency, timer for reliability

### Latency Characteristics

| Scenario | Latency |
|----------|---------|
| Send (ring has space) | <100ns (just reserve/commit) |
| Send (ring full, immediate drain) | ~1-10µs (notify + wake) |
| Send (ring full, wait for timer) | ≤`poll_interval` |
| Receive (data available) | Immediate |
| Receive (no data, event-driven) | Wake on notify |
| Receive (no data, timer fallback) | ≤`poll_interval` |

## Related Work

- **span_collector**: Async bridge pattern, graceful shutdown via oneshot
- **ringmpsc**: Lock-free ring buffer invariants, reserve/commit API
- **tokio_stream**: StreamExt combinators
- **futures**: Stream/Sink traits

## Testing Strategy

See [spec.md](spec.md) for invariant verification:

| Invariant | Verification |
|-----------|--------------|
| INV-STREAM-01 (FIFO) | FIFO ordering tests |
| INV-STREAM-02 (Hybrid) | Timer + notify interaction |
| INV-STREAM-03 (Backpressure) | debug_assert! on drain |
| INV-SINK-01 (No item loss) | try_send preservation |
| INV-SHUT-03/04 (Shutdown) | Graceful shutdown tests |
