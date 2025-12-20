# RingMPSC-RS

A high-performance lock-free Multi-Producer Single-Consumer (MPSC) channel implementation in Rust.

This is a Rust port of the [RingMPSC](https://github.com/boonzy00/ringmpsc) Zig implementation, maintaining the same design principles and algorithm while following Rust idioms.


## Algorithm

RingMPSC uses a **ring-decomposed** architecture where each producer has a dedicated SPSC (Single-Producer Single-Consumer) ring buffer. This eliminates producer-producer contention entirely.

```
Producer 0 ──► [Ring 0] ──┐
Producer 1 ──► [Ring 1] ──┼──► Consumer (polls all rings)
Producer 2 ──► [Ring 2] ──┤
Producer N ──► [Ring N] ──┘
```

### Key Optimizations

1. **128-byte Cache Line Alignment**: Head and tail pointers are separated by 128 bytes to prevent prefetcher-induced false sharing (Intel/AMD prefetchers may pull adjacent lines).

2. **Cached Sequence Numbers**: Producers cache the consumer's head position to minimize cross-core cache traffic. Cache refresh only occurs when the ring appears full.

3. **Batch Operations**: The `consume_all` API processes all available items with a single atomic head update, amortizing synchronization overhead.

4. **Adaptive Backoff**: Crossbeam-style exponential backoff (spin → yield → park) reduces contention without wasting CPU cycles.

5. **Zero-Copy API**: The `reserve`/`commit` pattern allows producers to write directly into the ring buffer without intermediate copies.

### Memory Layout

```
┌──────────────────────────────────────────────────────────────────┐
│ Producer Hot (128B aligned)                                      │
│   tail: AtomicU64        ← Producer writes, Consumer reads       │
│   cached_head: u64       ← Producer-local cache                  │
├──────────────────────────────────────────────────────────────────┤
│ Consumer Hot (128B aligned)                                      │
│   head: AtomicU64        ← Consumer writes, Producer reads       │
│   cached_tail: u64       ← Consumer-local cache                  │
├──────────────────────────────────────────────────────────────────┤
│ Cold State (128B aligned)                                        │
│   active, closed, metrics                                        │
├──────────────────────────────────────────────────────────────────┤
│ Data Buffer (64B aligned)                                        │
│   Vec<MaybeUninit<T>>                                            │
└──────────────────────────────────────────────────────────────────┘
```

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
ringmpsc-rs = "0.1"
```

### Basic Example

```rust
use ringmpsc_rs::{Channel, Config};

// Create channel with default config (64K slots, 16 max producers)
let channel = Channel::<u64>::new(Config::default());

// Register producer
let producer = channel.register().unwrap();

// Send (zero-copy)
if let Some(mut reservation) = producer.reserve(1) {
    reservation.as_mut_slice()[0] = 42;
    reservation.commit();
}

// Receive (batch)
let consumed = channel.consume_all(|item: &u64| {
    println!("Received: {}", item);
});
```

### Batch Processing Example

```rust
use ringmpsc_rs::{Channel, Config};

let channel = Channel::<u64>::new(Config::default());
let producer = channel.register().unwrap();

// Send multiple items
for i in 0..1000 {
    if let Some(mut r) = producer.reserve(1) {
        r.as_mut_slice()[0] = i;
        r.commit();
    }
}

// Process all available items with single atomic update
let mut sum = 0u64;
let consumed = channel.consume_all(|item| {
    sum += item;
});
println!("Consumed {} items, sum = {}", consumed, sum);
```

### Limited Batch for Real-World Processing

```rust
// Consume up to 1000 items at a time to avoid long pauses
let consumed = channel.consume_all_up_to(1000, |item| {
    // Do some work with item
    process_item(item);
});
```

### Multi-Producer Example

```rust
use std::thread;
use ringmpsc_rs::{Channel, Config};

let channel = Channel::<u64>::new(Config::default());
let channel2 = channel.clone();

// Producer 1
let p1 = channel.register().unwrap();
thread::spawn(move || {
    for i in 0..1000 {
        if let Some(mut r) = p1.reserve(1) {
            r.as_mut_slice()[0] = i;
            r.commit();
        }
    }
});

// Producer 2
let p2 = channel2.register().unwrap();
thread::spawn(move || {
    for i in 1000..2000 {
        if let Some(mut r) = p2.reserve(1) {
            r.as_mut_slice()[0] = i;
            r.commit();
        }
    }
});

// Consumer polls all rings
let consumed = channel.consume_all(|item| {
    println!("{}", item);
});
```

## Configuration

```rust
use ringmpsc_rs::{Config, LOW_LATENCY_CONFIG, HIGH_THROUGHPUT_CONFIG};

// Default configuration
let config = Config::default(); // 64K slots, 16 max producers

// Low latency (4K slots, fits in L1)
let config = LOW_LATENCY_CONFIG;

// High throughput (256K slots, 32 max producers)
let config = HIGH_THROUGHPUT_CONFIG;

// Custom configuration
let config = Config::new(
    14,    // ring_bits: 2^14 = 16K slots
    8,     // max_producers
    true,  // enable_metrics
);
```

## Benchmarks

Comprehensive throughput benchmarks are available using Criterion:

```bash
# Quick benchmark run
cargo bench --bench throughput -- --quick

# Full benchmark suite
cargo bench --bench throughput

# Run specific benchmark groups
cargo bench --bench throughput -- spsc
cargo bench --bench throughput -- mpsc
cargo bench --bench throughput -- batch_sizes
```

See [BENCHMARKS.md](BENCHMARKS.md) for detailed results and performance analysis.

**Quick Results** (on test system):
- SPSC: 1.46 billion elements/sec
- 2P2C: 1.42 billion elements/sec
- 4P4C: 1.23 billion elements/sec
- Best batch size: 16K (1.62 billion elements/sec)

## API Reference

### Channel

- `Channel::new(config)` - Create a new channel
- `register()` - Register a new producer (returns `Result<Producer, ChannelError>`)
- `consume_all(handler)` - Process all available items with batch update
- `consume_all_up_to(max, handler)` - Process up to max items
- `recv(&mut buffer)` - Convenience method to receive into buffer
- `close()` - Close the channel
- `is_closed()` - Check if closed
- `producer_count()` - Get number of registered producers
- `metrics()` - Get aggregated metrics (if enabled)

### Producer

- `reserve(n)` - Reserve n slots for zero-copy writing
- `reserve_with_backoff(n)` - Reserve with adaptive backoff
- `send(&items)` - Convenience method for batch send
- `close()` - Close this producer's ring
- `is_closed()` - Check if this producer's ring is closed

### Reservation

- `as_mut_slice()` - Get mutable slice for writing
- `commit()` - Commit all reserved slots
- `commit_n(n)` - Commit only n slots (n <= reserved)
- `len()` - Number of reserved slots
- `is_empty()` - Check if reservation is empty

## Correctness Properties

RingMPSC guarantees the following properties:

1. **Per-Producer FIFO**: Messages from a single producer are received in send order
2. **No Data Loss**: Every sent message is eventually received (assuming consumer runs)
3. **Thread Safety**: No data races under concurrent access
4. **Memory Safety**: Proper Drop implementation for cleanup

## Differences from Zig Implementation

While maintaining the same algorithm and design principles, this Rust implementation has some differences:

1. **Memory Safety**: Uses `MaybeUninit<T>` for uninitialized buffer slots with proper Drop handling
2. **Type Safety**: Strongly typed `Result` for error handling instead of Zig's error unions
3. **Lifetime Management**: Explicit lifetime annotations for `Reservation` references
4. **Trait System**: Implements `Send + Sync` traits with proper safety bounds
5. **Arc-based Sharing**: Uses `Arc` for cloning channels and producers instead of raw pointers
6. **No Comptime**: Runtime-based configuration instead of Zig's comptime generics

## Related Work

- [LMAX Disruptor](https://github.com/LMAX-Exchange/disruptor) - The original batch-consumption pattern
- [rigtorp/SPSCQueue](https://github.com/rigtorp/SPSCQueue) - High-performance C++ SPSC queue
- [crossbeam-channel](https://github.com/crossbeam-rs/crossbeam) - Rust concurrent channels
- [moodycamel::ConcurrentQueue](https://github.com/cameron314/concurrentqueue) - C++ lock-free queue
- [Original Zig Implementation](https://github.com/boonzy00/ringmpsc) - The source of this port

## License

MIT

## See Also

- [ALGORITHM.md](ALGORITHM.md) - Detailed algorithm description
- [Original Zig Implementation](https://github.com/boonzy00/ringmpsc)
