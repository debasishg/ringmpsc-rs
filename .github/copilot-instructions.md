# RingMPSC-RS Copilot Instructions

## Project Overview
A high-performance lock-free Multi-Producer Single-Consumer (MPSC) channel using **ring decomposition**: each producer gets a dedicated SPSC ring, eliminating all producer-producer contention. Rust port of the [Zig RingMPSC](https://github.com/boonzy00/ringmpsc) implementation, targeting 50+ billion messages/second.

## Architecture

### Core Components
- **[src/channel.rs](src/channel.rs)**: `Channel<T>` and `Producer<T>` - MPSC coordination layer
  - Contains `Vec<Ring<T>>` (one per producer) + registration logic
  - `Channel::consume_all()` is the **primary consumption API** - polls all rings with batch optimization
  - `Channel::register()` assigns producers to ring slots atomically (max 16-32 producers)
  
- **[src/ring.rs](src/ring.rs)**: `Ring<T>` - Lock-free SPSC ring buffer (the performance engine)
  - **128-byte cache-line alignment** on hot atomics (`tail`, `head`, `cached_head`, `cached_tail`)
  - Uses `MaybeUninit<T>` for buffer + proper Drop handling
  - **Cached sequence numbers**: producers/consumers cache remote counters to avoid cross-core reads
  - Circular buffer means reservations **may return fewer items than requested** when wrapping (see [src/ring.rs#L134-L150](src/ring.rs#L134-L150))

- **[src/reservation.rs](src/reservation.rs)**: Zero-copy write API
  - `reserve(n)` → `Reservation` → `commit()` eliminates intermediate copies
  - **CRITICAL**: Always check `reservation.as_mut_slice().len()` - may be < `n` due to wrap-around

- **[src/backoff.rs](src/backoff.rs)**: Adaptive retry (spin → yield → give up)

### Key Patterns

#### Memory Ordering
- **Producer write path**: `Ordering::Relaxed` on cached head, `Ordering::Acquire` on slow-path refresh, `Ordering::Release` on tail update
- **Consumer read path**: `Ordering::Acquire` on tail read, `Ordering::Release` on head update
- See [src/ring.rs#L161](src/ring.rs#L161) for producer logic and [src/ring.rs#L312](src/ring.rs#L312) for consumer batch logic

#### Batch Consumption (Disruptor Pattern)
`consume_batch()` / `consume_all()` process N items with **one atomic head update** (vs N updates in traditional channels). This is the main performance win. See [src/ring.rs#L312-L345](src/ring.rs#L312-L345).

#### Per-Producer FIFO Guarantee
Each `Ring<T>` maintains FIFO order for its producer. Multi-producer scenarios have no global order guarantee - only per-producer order. Test: [tests/integration_tests.rs#L30-L70](tests/integration_tests.rs#L30-L70).

## Development Workflows

### Build & Test
```bash
cargo build --release      # Release builds (50x faster than debug for lock-free code)
cargo test                 # Run all unit + integration tests
cargo test -- --nocapture  # See println! output during tests
```

### Benchmarking
```bash
cargo bench                           # Run all criterion benchmarks
cargo bench --bench throughput spsc   # Run specific benchmark group
open target/criterion/report/index.html  # View HTML reports
```
Benchmarks in [benches/throughput.rs](benches/throughput.rs) use criterion + 10M message batches. Measure SPSC (single pair) and MPSC (2/4/8 producers).

### Examples
```bash
cargo run --release --example basic              # Multi-producer demo
cargo run --release --example zero_copy          # Reservation API patterns
cargo run --release --example scaling_benchmark  # Performance scaling test
```

## Project-Specific Conventions

### Safety & Unsafety
- `unsafe` blocks appear in [src/ring.rs](src/ring.rs) for `MaybeUninit<T>` manipulation and pointer casting
- All unsafe code has safety comments explaining invariants (see [src/ring.rs#L200-L210](src/ring.rs#L200-L210))
- `UnsafeCell` used for interior mutability on cached sequence numbers (single-writer guarantee)

### Configuration
- `Config::ring_bits` sets capacity as power-of-2 (default 16 = 64K slots)
- **Validation**: `ring_bits` must be 1-20 (max 1M slots), `max_producers` must be 1-128
- `LOW_LATENCY_CONFIG` (12 bits = 4K, fits L1 cache) vs `HIGH_THROUGHPUT_CONFIG` (18 bits = 256K)
- See [src/config.rs](src/config.rs) for presets

### Error Handling
- `reserve()` returns `Option<Reservation>` (None = full or closed)
- `register()` returns `Result<Producer, ChannelError>` (TooManyProducers | Closed)
- No panics in hot paths; assert-style panics only in commit validation

### Testing Patterns
- **FIFO tests**: Verify per-producer order with `(producer_id, sequence)` tuples (see [tests/integration_tests.rs#L30](tests/integration_tests.rs#L30))
- **Stress tests**: 8+ producers × 50K items with concurrent threads (see [tests/integration_tests.rs#L77](tests/integration_tests.rs#L77))
- **No data loss**: Sum all values and compare to expected total

## Common Gotchas

1. **Reservation wrap-around**: `reserve(1000)` may return a slice with `len() < 1000`. Loop until fully reserved:
   ```rust
   let mut remaining = n;
   while remaining > 0 {
       if let Some(mut r) = producer.reserve(remaining) {
           let got = r.as_mut_slice().len();
           // ... write to slice ...
           r.commit();
           remaining -= got;
       }
   }
   ```

2. **Debug builds are unusable**: Lock-free algorithms require release optimizations. Always benchmark with `--release`.

3. **Backpressure handling**: `reserve()` returns None when full. Producer must retry with `reserve_with_backoff()` or custom logic (see [examples/basic.rs#L32](examples/basic.rs#L32)).

4. **Drop safety**: Ring's Drop implementation properly drops all unconsumed items using `MaybeUninit::assume_init_drop()`.

## When Adding Features

- Maintain 128-byte alignment on new hot fields (use `CacheAligned<T>` wrapper)
- Add metrics conditionally behind `config.enable_metrics` flag (slight overhead)
- Preserve lock-free properties: no mutexes, no blocking operations in hot paths
- Follow the "amortize atomics" principle: batch operations > single-item operations
- **No software prefetch**: Hardware prefetchers handle sequential access better on modern CPUs
