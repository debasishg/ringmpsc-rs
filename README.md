# RingMPSC-RS

A high-performance lock-free Multi-Producer Single-Consumer (MPSC) channel implementation in Rust.

This is a Rust port of the [RingMPSC](https://github.com/boonzy00/ringmpsc) Zig implementation, maintaining the same design principles and algorithm while following Rust idioms.

This is a work in progress (not safe at all to use for any serious application).


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

## TODO

### Future Optimizations

- [ ] **NUMA-aware ring allocation** - Allocate ring buffers on NUMA nodes local to their producer/consumer threads for multi-socket systems
- [ ] **Custom allocator integration** - Allow users to provide custom allocators for specialized use cases (arena allocators, huge pages, etc.)
- [ ] **Optional StackRing variant** - Provide a stack-allocated ring buffer variant behind a feature flag for latency-critical expert use. See [STACK_RING_IMPL.md](STACK_RING_IMPL.md) for the implementation plan and design details.

### Feature Flags

```toml
[dependencies]
ringmpsc-rs = { version = "0.1", features = ["stack-ring"] }
```

| Feature | Description |
|---------|-------------|
| `stack-ring` | Enables `StackRing<T, N>` and `StackChannel<T, N, P>` — stack-allocated variants for latency-critical use |

## License

MIT