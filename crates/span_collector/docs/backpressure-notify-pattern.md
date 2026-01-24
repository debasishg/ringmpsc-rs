# Async Signaling Patterns with `tokio::sync::Notify`

## Overview

`tokio::sync::Notify` is a lightweight synchronization primitive in Tokio that provides a simple mechanism for **async task notification**. Unlike channels that transfer data, `Notify` purely signals that "something happened" — making it ideal for coordination patterns like backpressure, state changes, and event-driven wake-ups.

---

## Part 1: The Generic Pattern

### What is `tokio::sync::Notify`?

`Notify` provides a **single-producer, multi-consumer** notification mechanism:

- **No data is transferred** — it's purely a signal
- **Waiters are suspended** until notified
- **Lightweight** — no allocation per notification

### Core API

| Method | Description |
|--------|-------------|
| `Notify::new()` | Creates a new `Notify` instance |
| `notified().await` | Waits for a notification (suspends the task) |
| `notify_one()` | Wakes exactly one waiting task |
| `notify_waiters()` | Wakes **all** currently waiting tasks |

### The Backpressure Pattern

Backpressure is a flow control mechanism where a **consumer signals producers** when it's ready to accept more work. This prevents overwhelming the consumer and manages memory usage.

#### Generic Structure

```rust
use std::sync::Arc;
use tokio::sync::Notify;

struct Producer {
    buffer: SomeBuffer,
    backpressure: Arc<Notify>,
}

struct Consumer {
    buffer: SomeBuffer,
    backpressure: Arc<Notify>,
}

impl Producer {
    async fn send(&self, item: Item) {
        loop {
            match self.buffer.try_push(item.clone()) {
                Ok(()) => return,
                Err(BufferFull) => {
                    // Wait for consumer to signal space available
                    self.backpressure.notified().await;
                }
            }
        }
    }
}

impl Consumer {
    async fn consume(&self) {
        loop {
            let consumed = self.buffer.drain();
            
            if consumed > 0 {
                // Signal all waiting producers that space is available
                self.backpressure.notify_waiters();
            }
            
            // ... process items ...
        }
    }
}
```

#### Key Design Decisions

1. **`notify_waiters()` vs `notify_one()`**
   - Use `notify_waiters()` when **multiple producers** might be waiting
   - Use `notify_one()` for single-producer scenarios or fair scheduling

2. **Loop with retry**
   - After being notified, the producer **retries** the operation
   - Another producer might have taken the slot — the loop handles this race

3. **Shared via `Arc`**
   - Both producer and consumer hold `Arc<Notify>` references
   - Enables multiple producers to share the same notification channel

### Important Semantics

#### Notification Permits

`Notify` maintains a **single permit**:

```rust
let notify = Notify::new();

// This stores a permit
notify.notify_one();

// This consumes the permit immediately (doesn't wait)
notify.notified().await;
```

If `notify_one()` is called before anyone is waiting, the permit is stored and the next `notified().await` returns immediately.

#### No Permit Accumulation

Permits **don't accumulate**:

```rust
notify.notify_one();
notify.notify_one(); // This is effectively a no-op

notify.notified().await; // Consumes the one permit
notify.notified().await; // This WILL wait
```

### Other Use Cases for `Notify`

#### 1. Graceful Shutdown Signal

```rust
struct Server {
    shutdown: Arc<Notify>,
}

impl Server {
    async fn run(&self) {
        loop {
            tokio::select! {
                conn = self.accept() => {
                    self.handle(conn).await;
                }
                _ = self.shutdown.notified() => {
                    println!("Shutdown signal received");
                    break;
                }
            }
        }
    }
    
    fn trigger_shutdown(&self) {
        self.shutdown.notify_waiters();
    }
}
```

#### 2. State Change Notification

```rust
struct StateManager {
    state: Mutex<AppState>,
    state_changed: Notify,
}

impl StateManager {
    async fn wait_for_ready(&self) {
        loop {
            {
                let state = self.state.lock().unwrap();
                if state.is_ready() {
                    return;
                }
            }
            self.state_changed.notified().await;
        }
    }
    
    fn set_ready(&self) {
        {
            let mut state = self.state.lock().unwrap();
            state.set_ready(true);
        }
        self.state_changed.notify_waiters();
    }
}
```

#### 3. Periodic Batch Flushing with Manual Trigger

```rust
struct BatchWriter {
    batch: Mutex<Vec<Item>>,
    flush_trigger: Notify,
}

impl BatchWriter {
    async fn flush_loop(&self) {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    self.flush().await;
                }
                _ = self.flush_trigger.notified() => {
                    self.flush().await;
                }
            }
        }
    }
    
    fn trigger_immediate_flush(&self) {
        self.flush_trigger.notify_one();
    }
}
```

#### 4. Resource Pool Availability

```rust
struct ConnectionPool {
    connections: Mutex<Vec<Connection>>,
    available: Notify,
}

impl ConnectionPool {
    async fn acquire(&self) -> Connection {
        loop {
            {
                let mut conns = self.connections.lock().unwrap();
                if let Some(conn) = conns.pop() {
                    return conn;
                }
            }
            self.available.notified().await;
        }
    }
    
    fn release(&self, conn: Connection) {
        {
            let mut conns = self.connections.lock().unwrap();
            conns.push(conn);
        }
        self.available.notify_one();
    }
}
```

---

## Part 2: Implementation in AsyncSpanCollector

### Context

The `AsyncSpanCollector` bridges synchronous MPSC ring buffer channels with async Rust. Multiple producers submit spans to per-producer ring buffers, while a single consumer task drains these buffers and exports spans in batches.

### The Challenge

When a producer's ring buffer is **full**, the producer cannot submit more spans. Without proper signaling:
- **Busy-looping** wastes CPU cycles
- **Synchronous blocking** defeats the purpose of async code
- **Dropping spans** loses observability data

### Solution Architecture

```
┌──────────────┐     ┌──────────────┐     ┌──────────────┐
│  Producer 1  │     │  Producer 2  │     │  Producer N  │
│              │     │              │     │              │
│ Ring Buffer  │     │ Ring Buffer  │     │ Ring Buffer  │
└──────┬───────┘     └──────┬───────┘     └──────┬───────┘
       │                    │                    │
       │    try_submit      │                    │
       │    ────────►       │                    │
       │                    │                    │
       │    Full?           │                    │
       │    notified()      │                    │
       │    .await          │                    │
       │         ▲          │                    │
       │         │          │                    │
       │         │ notify_waiters()              │
       │         │          │                    │
       └─────────┼──────────┴────────────────────┘
                 │
        ┌────────┴────────┐
        │  Consumer Task  │
        │                 │
        │  consume_all()  │
        │  batch + export │
        └─────────────────┘
```

### Implementation Details

#### Collector Structure

The `AsyncSpanCollector` holds the shared `Notify`:

```rust
pub struct AsyncSpanCollector {
    collector: Arc<SpanCollector>,
    consumer_task: Option<JoinHandle<()>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    backpressure_notify: Arc<Notify>,
}
```

#### Initialization

During construction, the `Notify` is created and shared with the consumer task:

```rust
pub async fn new(config: AsyncCollectorConfig, exporter: Arc<dyn SpanExporter>) -> Self {
    let collector = Arc::new(SpanCollector::new(config.collector_config));
    let backpressure_notify = Arc::new(Notify::new());

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    // Clone for the consumer task
    let collector_clone = Arc::clone(&collector);
    let backpressure_clone = Arc::clone(&backpressure_notify);
    
    // ... spawn consumer task with backpressure_clone ...
}
```

#### Consumer Task — The Notifier

The consumer task runs on a periodic interval, drains spans from all ring buffers, and signals producers when space is freed:

```rust
let consumer_task = tokio::spawn(async move {
    let mut interval = tokio::time::interval(consumer_interval);
    let mut batch_processor = BatchProcessor::new(config.batch_config, exporter);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // Consume spans from ring buffers
                let consumed = collector_clone.consume_all_up_to(max_consume, |span| {
                    batch_processor.add(span.clone());
                });

                if consumed > 0 {
                    // Notify waiting producers that space is available
                    backpressure_clone.notify_waiters();
                }

                // Flush batch if needed
                if batch_processor.should_flush() {
                    if let Err(e) = batch_processor.flush().await {
                        eprintln!("Export error: {}", e);
                    }
                }
            }
            _ = &mut shutdown_rx => {
                // ... shutdown logic ...
                break;
            }
        }
    }
});
```

**Key points:**
- Only notifies when `consumed > 0` (optimization — no spurious wake-ups)
- Uses `notify_waiters()` to wake **all** waiting producers simultaneously

#### Producer Structure

Each producer receives its own handle with a reference to the shared `Notify`:

```rust
pub struct AsyncSpanProducer {
    producer: crate::collector::SpanProducer,
    backpressure_notify: Arc<Notify>,
}
```

#### Producer Registration

When a new producer is registered, it receives a clone of the `Notify`:

```rust
pub async fn register_producer(&self) -> Result<AsyncSpanProducer, AsyncError> {
    let producer = self.collector.register()
        .map_err(|e| AsyncError::RegistrationFailed(e.to_string()))?;
    Ok(AsyncSpanProducer {
        producer,
        backpressure_notify: Arc::clone(&self.backpressure_notify),
    })
}
```

#### Submitting Spans — The Waiter

The async `submit_span` method implements the backpressure wait loop:

```rust
impl AsyncSpanProducer {
    pub async fn submit_span(&self, span: Span) -> Result<(), SubmitError> {
        loop {
            match self.producer.try_submit_span(span.clone()) {
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

**Key points:**
- **Non-blocking try**: Uses `try_submit_span` which returns immediately
- **Async wait on full**: Suspends the task via `notified().await`
- **Retry loop**: After wake-up, retries the submission (another producer may have filled the space)
- **Clone on retry**: The span is cloned because it might be needed for multiple attempts

#### Non-blocking Alternative

For scenarios where waiting is not acceptable, a non-blocking variant is also provided:

```rust
pub fn try_submit_span(&self, span: Span) -> Result<(), SubmitError> {
    self.producer.try_submit_span(span)
}
```

### Why This Design Works

| Aspect | Benefit |
|--------|---------|
| **Non-blocking** | Producers yield to the Tokio runtime instead of spinning |
| **Efficient** | Only wakes when there's actually space available |
| **Scalable** | Multiple producers share one `Notify` — O(1) memory |
| **Fair** | All waiting producers wake simultaneously and compete fairly |
| **Composable** | Works seamlessly with `tokio::select!` and other async primitives |

### Race Condition Handling

The retry loop handles an important race:

1. Producer A calls `notified().await` (ring is full)
2. Consumer drains 100 spans, calls `notify_waiters()`
3. Producer A wakes up
4. Producer B (not waiting) submits a span, filling the last slot
5. Producer A's `try_submit_span` returns `Full` again
6. Producer A loops back and waits again

Without the loop, Producer A would incorrectly assume space is available after being notified.

---

## Summary

`tokio::sync::Notify` is a versatile primitive for async coordination patterns:

- **Backpressure**: Consumers signal producers when ready for more work
- **Shutdown**: Broadcast termination signals to multiple tasks
- **State changes**: Wake tasks waiting for specific conditions
- **Resource availability**: Signal when pooled resources become free

The key insight is that `Notify` separates **signaling** from **data transfer**, making it lightweight and flexible for pure coordination scenarios.
