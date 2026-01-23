# RingMPSC-RS Copilot Instructions

## Project Overview
A high-performance lock-free MPSC channel using **ring decomposition**: each producer gets a dedicated SPSC ring, eliminating producer-producer contention. Rust port of [RingMPSC Zig](https://github.com/boonzy00/ringmpsc), targeting 50+ billion msg/sec.

## Architecture

### Core Components
| Module | Purpose |
|--------|---------|
| [src/channel.rs](src/channel.rs) | `Channel<T>`, `Producer<T>` - MPSC coordination, `consume_all()` polls all rings |
| [src/ring.rs](src/ring.rs) | `Ring<T>` - SPSC ring buffer with 128-byte aligned atomics, `MaybeUninit<T>` buffer |
| [src/reservation.rs](src/reservation.rs) | Zero-copy `reserve(n)` → `Reservation` → `commit()` API |
| [src/stack_ring.rs](src/stack_ring.rs) | `StackRing<T, N>` - Stack-allocated variant (feature `stack-ring`) |
| [src/config.rs](src/config.rs) | `Config`, `LOW_LATENCY_CONFIG`, `HIGH_THROUGHPUT_CONFIG` presets |

### Key Design Decisions
- **Unbounded u64 sequence numbers** prevent ABA (58 years to wrap at 10B msg/sec)
- **Cached head/tail** in `UnsafeCell`: single-writer fields avoid cross-core reads ([src/ring.rs#L56-L68](src/ring.rs#L56-L68))
- **Batch consumption** (`consume_batch`): one atomic head update for N items - Disruptor pattern ([src/ring.rs#L378](src/ring.rs#L378))
- **Per-producer FIFO only**: no global ordering across producers

### Reference Implementation: Span Collector

The [examples/span_collector/](examples/span_collector/) is a complete async OpenTelemetry tracing collector showcasing all library patterns.

#### Architecture Layers
| Layer | File | Purpose |
|-------|------|---------|
| Sync Core | [collector.rs](examples/span_collector/src/collector.rs) | `SpanCollector`, `SpanProducer` - wraps `Channel<Span>` |
| Async Bridge | [async_bridge.rs](examples/span_collector/src/async_bridge.rs) | Bridges ringmpsc → tokio via `Notify` |
| Batch Processing | [batch_processor.rs](examples/span_collector/src/batch_processor.rs) | Groups spans by trace_id, time/size-based flush |
| Export | [exporter.rs](examples/span_collector/src/exporter.rs) | OTLP/stdout/JSON export with retry |

#### Key Design Decisions

1. **Sync Core + Async Bridge Pattern**
   - Keep lock-free ring operations synchronous for predictability
   - Bridge to async via `tokio::spawn` consumer task + `tokio::sync::Notify`
   - Never block tokio runtime threads with spin loops

2. **Owned Consumption Only** ([collector.rs#L140](examples/span_collector/src/collector.rs#L140))
   ```rust
   // CORRECT: owned consumption - zero-copy transfer
   collector.consume_all_up_to_owned(limit, |span: Span| { ... });
   
   // NOT PROVIDED: reference consumption would force clone
   // collector.consume_all_up_to(limit, |span: &Span| { ... }); // Span has String, Box<HashMap>
   ```
   `Span` contains heap allocations; owned APIs avoid cloning.

3. **Bounded Consumption per Poll** ([async_bridge.rs#L74](examples/span_collector/src/async_bridge.rs#L74))
   ```rust
   let consumed = collector.consume_all_up_to(10_000, |span| ...);
   ```
   Prevents consumer task from hogging the runtime; ensures predictable latency.

4. **Boxed Variable-Size Fields** ([span.rs](examples/span_collector/src/span.rs))
   ```rust
   pub struct Span {
       pub trace_id: u128,      // Fixed
       pub attributes: Box<HashMap<String, AttributeValue>>, // Boxed!
   }
   ```
   Keeps `Span` size ~88 bytes for efficient ring storage; heap-indirection for variable data.

#### Backpressure Pattern with `tokio::sync::Notify`

```rust
// Producer side (async_bridge.rs)
impl AsyncSpanProducer {
    pub async fn submit_span(&self, span: Span) -> Result<(), SubmitError> {
        loop {
            match self.producer.submit_span(span.clone()) {
                Ok(()) => return Ok(()),
                Err(SubmitError::Full) => {
                    // Wait for consumer to signal space available
                    self.backpressure_notify.notified().await;
                }
            }
        }
    }
}

// Consumer side (async_bridge.rs#L76-L83)
let consumed = collector.consume_all_up_to(max_consume, |span| { ... });
if consumed > 0 {
    // Signal ALL waiting producers that space is available
    backpressure_notify.notify_waiters();
}
```

**Why `notify_waiters()` not `notify_one()`**: Multiple producers may be waiting; wake all to retry.

#### Graceful Shutdown Protocol
```rust
// 1. Signal shutdown
shutdown_tx.send(()).ok();

// 2. Consumer task receives signal, drains remaining spans
collector.consume_all(|span| batch_processor.add(span));

// 3. Final flush to exporter
batch_processor.flush().await?;
```

#### Metrics Pattern (Relaxed Atomics)
```rust
// All counters use Relaxed ordering - safe because:
// - No control flow depends on values
// - No happens-before relationships needed
// - Eventual consistency acceptable for observability
metrics.spans_submitted.fetch_add(1, Ordering::Relaxed);
```

See [examples/span_collector/docs/backpressure-notify-pattern.md](examples/span_collector/docs/backpressure-notify-pattern.md) for the full async signaling pattern.

## Development Workflows

```bash
# Build & Test
cargo build --release           # Always use --release for lock-free code
cargo test                      # Unit + integration tests
cargo test --features loom --test loom_tests --release   # Exhaustive concurrency testing
cargo +nightly miri test --test miri_tests               # UB detection

# Benchmarking (10M messages, criterion)
cargo bench                           # All benchmarks
cargo bench --bench throughput spsc   # Specific group

# Stack-allocated variants
cargo test --features stack-ring
cargo bench --features stack-ring --bench stack_vs_heap
```

## Coding Patterns

### Reservation API (Zero-Copy Writes)
**CRITICAL**: `reserve(n)` may return fewer items than requested due to wrap-around:
```rust
let mut remaining = n;
while remaining > 0 {
    if let Some(mut r) = producer.reserve(remaining) {
        let slice = r.as_mut_slice();
        let got = slice.len(); // MAY BE < remaining!
        for slot in slice.iter_mut() {
            slot.write(value);  // Use MaybeUninit::write()
        }
        r.commit();
        remaining -= got;
    } else {
        thread::yield_now(); // Backpressure: ring full
    }
}
```

### Simple API (Single Items)
```rust
// Convenience method - returns false if ring full
while !producer.push(value) { thread::yield_now(); }
```

### Stack vs Heap Ring Selection
| Use Case | Choice | Reason |
|----------|--------|--------|
| Embedded/real-time, no allocator | `StackRing<T, N>` | Zero heap allocation |
| Predictable cache locality | `StackRing<T, 4096>` | Buffer inline, fits L2 |
| Large buffers (>64K slots) | `Ring<T>` | Avoids stack overflow |
| Dynamic capacity needed | `Ring<T>` | Runtime `Config::ring_bits` |

**Size limits** ([src/stack_ring.rs#L44-L48](src/stack_ring.rs#L44-L48)):
- `StackRing<u64, 4096>` ≈ 33KB ✓ safe everywhere
- `StackRing<u64, 65536>` ≈ 524KB ⚠️ may overflow thread stacks

### Memory Ordering Protocol
- **Producer**: `Relaxed` on tail load, `Acquire` on head refresh, `Release` on tail store
- **Consumer**: `Acquire` on tail read, `Release` on head update
- All unsafe code has `// Safety:` comments explaining invariants

### Configuration
- `ring_bits`: 1-20 (capacity = 2^bits, default 16 = 64K slots)
- `max_producers`: 1-128 (default 16)
- `enable_metrics`: slight overhead, off by default

### Async Integration Pattern (from span_collector)
When integrating ringmpsc with async runtimes:

```rust
// 1. Keep sync core - don't make ring operations async
pub struct SyncCollector {
    channel: Arc<Channel<T>>,
}

// 2. Bridge to async via spawn + Notify
pub struct AsyncCollector {
    sync_collector: Arc<SyncCollector>,
    consumer_task: JoinHandle<()>,
    backpressure: Arc<Notify>,
}

// 3. Consumer task polls at fixed interval (don't spin!)
let consumer_task = tokio::spawn(async move {
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let consumed = collector.consume_all_up_to(10_000, handler);
                if consumed > 0 { notify.notify_waiters(); }
            }
            _ = &mut shutdown_rx => break,
        }
    }
});

// 4. Async producer awaits notification (doesn't spin!)
pub async fn submit(&self, item: T) -> Result<(), Error> {
    loop {
        match self.try_submit(item.clone()) {
            Ok(()) => return Ok(()),
            Err(Full) => self.backpressure.notified().await,
        }
    }
}
```

### Data Structure Design for Ring Buffers
When designing types for ring buffer storage:

| Pattern | Example | Benefit |
|---------|---------|---------|
| Fixed-size core | `trace_id: u128, span_id: u64` | Predictable ring slot size |
| Boxed variable data | `attributes: Box<HashMap<K,V>>` | Keeps struct small (~88 bytes) |
| Owned strings | `name: String` (not `&str`) | No lifetime params, safe transfer |
| Atomic counters | `AtomicU64` with `Relaxed` | Low-overhead observability |

## Testing Conventions

- **FIFO verification**: Use `(producer_id, sequence)` tuples, verify per-producer order ([tests/integration_tests.rs#L30](tests/integration_tests.rs#L30))
- **Stress tests**: 8+ producers × 50K+ items, verify sum equals expected ([tests/integration_tests.rs#L77](tests/integration_tests.rs#L77))
- **Loom tests**: Simplified ring in [tests/loom_tests.rs](tests/loom_tests.rs) for exhaustive interleaving
- **Miri tests**: [tests/miri_tests.rs](tests/miri_tests.rs) catches UB in unsafe paths

### Span Collector Testing Patterns

```bash
# Run span_collector tests
cd examples/span_collector
cargo test                    # Unit + integration tests
cargo test --release          # Performance-sensitive tests
```

- **Ownership transfer**: Verify spans are moved (not cloned) through pipeline
- **Backpressure**: Test slow consumer scenario - producers should await, not spin
- **Graceful shutdown**: Verify all in-flight spans are drained and exported
- **Metrics conservation**: `spans_submitted == spans_consumed` after full drain

## Common Gotchas

1. **Debug builds are broken**: Lock-free code requires `--release` optimizations
2. **Reservation wrap-around**: Always check `slice.len()`, loop if needed
3. **`MaybeUninit` writes**: Use `slot.write(value)` not direct assignment
4. **Backpressure**: `reserve()`/`push()` returns `None`/`false` when full - caller must retry

## Adding Features Checklist

- [ ] Use `CacheAligned<T>` wrapper for hot atomic fields (128-byte alignment)
- [ ] Add `// Safety:` comments on all unsafe blocks
- [ ] Gate metrics behind `config.enable_metrics`
- [ ] No mutexes or blocking in hot paths
- [ ] Batch operations > single-item operations
- [ ] Test with loom (`--features loom`) and miri (`+nightly miri`)
- [ ] Verify invariants in [specs/](../specs/) are preserved

## Specifications

Formal invariants that implementations must satisfy:
- [specs/ring-buffer-invariants.md](../specs/ring-buffer-invariants.md) - Memory layout, sequencing, ordering, drop safety
- [examples/span_collector/specs/invariants.md](../examples/span_collector/specs/invariants.md) - Domain model, ownership transfer, backpressure
