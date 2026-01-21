# StackRing Implementation Plan

This document describes the design and implementation of the optional stack-allocated ring buffer variants for RingMPSC-RS, enabled via the `stack-ring` feature flag.

## Motivation

The default `Ring<T>` and `Channel<T>` use heap allocation for maximum flexibility:
- Runtime-configurable capacity via `Config::ring_bits`
- Dynamic producer registration with `Channel::register()`
- Arc-based sharing across threads

However, heap allocation introduces:
1. **Pointer indirection** — Every buffer access requires dereferencing `buffer_ptr`
2. **Allocation latency** — Initial `alloc()` call during construction
3. **Cache unpredictability** — Buffer may land anywhere in memory

For **latency-critical expert use**, a stack-allocated variant eliminates these costs by embedding the buffer directly in the struct, making `buffer[idx]` a simple base+offset calculation that the compiler can constant-fold.

## Architecture Overview

### Phase 1: StackRing (SPSC Only)

A standalone single-producer single-consumer ring buffer with compile-time capacity:

```rust
#[repr(C)]
pub struct StackRing<T, const N: usize> {
    // === Producer hot path (cache line 1) ===
    tail: AtomicU64,
    cached_head: UnsafeCell<u64>,

    // === Consumer hot path (cache line 2) ===
    head: CacheLinePadded<AtomicU64>,
    cached_tail: UnsafeCell<u64>,

    // === Cold state ===
    closed: AtomicBool,

    // === Buffer (inline, no pointer indirection) ===
    buffer: [UnsafeCell<MaybeUninit<T>>; N],
}
```

**Key differences from `Ring<T>`:**

| Aspect | `Ring<T>` | `StackRing<T, N>` |
|--------|-----------|-------------------|
| Buffer storage | `Box<[MaybeUninit<T>]>` (heap) | `[UnsafeCell<MaybeUninit<T>>; N]` (inline) |
| Capacity | Runtime (`Config::ring_bits`) | Compile-time const generic `N` |
| Size constraint | Up to 1M slots | Practical limit ~16K (stack size) |
| Pointer loads | 1 per access (`buffer_ptr`) | 0 (constant offset) |

### Phase 2: StackChannel (MPSC)

A fully stack-allocated multi-producer single-consumer channel using ring decomposition:

```
┌─────────────────────────────────────────────────────────────────────┐
│                     StackChannel<T, N, P>                           │
├─────────────────────────────────────────────────────────────────────┤
│                                                                     │
│  Producer 0 ──► [StackRing 0] ──┐                                   │
│  Producer 1 ──► [StackRing 1] ──┼──► ONE Consumer (consume_all)     │
│  Producer 2 ──► [StackRing 2] ──┤    polls all rings in sequence    │
│      ...                        │                                   │
│  Producer P ──► [StackRing P] ──┘                                   │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

**Key architecture points:**
- Each producer gets a **dedicated SPSC ring** — no producer-producer contention
- **Single consumer** calls `consume_all()` which iterates through all active rings
- This is **MPSC = [SPSC] × P**, NOT P independent SPSC pairs

```rust
#[repr(C)]
pub struct StackChannel<T, const N: usize, const P: usize> {
    rings: [StackRing<T, N>; P],
    producer_count: AtomicUsize,
    closed: AtomicBool,
}

pub struct StackProducer<'a, T, const N: usize, const P: usize> {
    channel: &'a StackChannel<T, N, P>,
    id: usize,
}
```

**Why two const generics?**
- `N` — Ring capacity (must be power of 2)
- `P` — Maximum producer count (array size)

This mirrors the Zig implementation's comptime design where both values are known at compile time.

## API Reference

### StackRing<T, N>

```rust
impl<T, const N: usize> StackRing<T, N> {
    /// Create a new stack-allocated ring.
    /// Panics at compile time if N is not a power of 2.
    pub const fn new() -> Self;

    /// Reserve space for writing n elements.
    /// Returns pointer and actual reserved length (may be < n due to wrap).
    pub unsafe fn reserve(&self, n: usize) -> Option<(*mut T, usize)>;

    /// Commit n elements that were written.
    pub fn commit(&self, n: usize);

    /// Peek at available data for reading.
    pub unsafe fn peek(&self) -> (*const T, usize);

    /// Advance the read pointer by n elements.
    pub fn advance(&self, n: usize);

    /// Consume all available items in batch (single atomic head update).
    pub unsafe fn consume_batch<F: FnMut(&T)>(&self, handler: F) -> usize;

    /// Check if the ring is empty/closed.
    pub fn is_empty(&self) -> bool;
    pub fn is_closed(&self) -> bool;
    pub fn close(&self);
}
```

### StackChannel<T, N, P>

```rust
impl<T, const N: usize, const P: usize> StackChannel<T, N, P> {
    /// Create a new stack-allocated channel.
    pub const fn new() -> Self;

    /// Register a producer. Returns error if P producers already registered.
    pub fn register(&self) -> Result<StackProducer<'_, T, N, P>, ChannelError>;

    /// Consume from all active producers.
    pub fn consume_all<F: FnMut(&T)>(&self, handler: F) -> usize;

    /// Close the channel and all rings.
    pub fn close(&self);
}
```

## Usage Examples

### Basic SPSC (StackRing)

```rust
use ringmpsc_rs::StackRing;

// 4K slots, stack-allocated
let ring: StackRing<u64, 4096> = StackRing::new();

// Producer
unsafe {
    if let Some((ptr, len)) = ring.reserve(10) {
        for i in 0..len {
            *ptr.add(i) = i as u64;
        }
        ring.commit(len);
    }
}

// Consumer
unsafe {
    ring.consume_batch(|value| {
        println!("Got: {}", value);
    });
}
```

### Multi-Producer (StackChannel)

```rust
use ringmpsc_rs::{StackChannel, StackProducer};
use std::thread;

// 4K slots per ring, max 4 producers
let channel: StackChannel<u64, 4096, 4> = StackChannel::new();

// Leak to get 'static lifetime for thread sharing
let channel: &'static _ = Box::leak(Box::new(channel));

let handles: Vec<_> = (0..4).map(|i| {
    let producer = channel.register().unwrap();
    thread::spawn(move || {
        unsafe {
            if let Some((ptr, len)) = producer.reserve(100) {
                for j in 0..len {
                    *ptr.add(j) = (i * 1000 + j) as u64;
                }
                producer.commit(len);
            }
        }
    })
}).collect();

// Consumer on main thread
let total = channel.consume_all(|v| println!("{}", v));
```

### Type Aliases for Common Configurations

```rust
/// 4K slots - fits in L1 cache, ideal for low latency
pub type StackRing4K<T> = StackRing<T, 4096>;

/// 16K slots - fits in L2 cache, good balance
pub type StackRing16K<T> = StackRing<T, 16384>;

/// 4K slots, 16 producers - matches default Config
pub type StackChannel4K16P<T> = StackChannel<T, 4096, 16>;
```

## Size Constraints & Warnings

### Stack Overflow Risk

A `StackRing<u64, 16384>` uses ~131KB of stack space (16K × 8 bytes). Default thread stack sizes:
- Linux: 8MB (safe)
- macOS: 512KB (safe for ≤64K slots with small T)
- Windows: 1MB (safe for ≤128K slots with small T)

**Guidelines:**
- `N ≤ 4096` — Safe for any platform
- `N ≤ 16384` — Safe for most platforms with `T` ≤ 64 bytes
- `N > 16384` — Consider heap `Ring<T>` or increase thread stack size

### StackChannel Memory Layout

`StackChannel<T, 4096, 16>` with `T = u64` uses:
- 16 × StackRing = 16 × ~33KB = ~528KB
- Plus atomics overhead = ~530KB total

This is substantial! For >4 producers or large T, prefer heap-based `Channel<T>`.

## When to Use Which

| Scenario | Recommended Type |
|----------|------------------|
| General purpose MPSC | `Channel<T>` |
| Latency-critical SPSC | `StackRing<T, N>` |
| Latency-critical MPSC, few producers | `StackChannel<T, N, P>` |
| Dynamic producer count | `Channel<T>` |
| Configurable capacity at runtime | `Channel<T>` / `Ring<T>` |
| Embedded/no-std (future) | `StackRing<T, N>` |

## Implementation Checklist

### Phase 1: StackRing ✅ COMPLETE
- [x] Add `stack-ring` feature flag to Cargo.toml
- [x] Create `src/stack_ring.rs` with `StackRing<T, N>`
- [x] Implement `CacheLinePadded<T>` wrapper (128-byte alignment)
- [x] Implement `reserve()`, `commit()`, `peek()`, `advance()`
- [x] Implement `consume_batch()` with single atomic head update
- [x] Add compile-time power-of-2 assertion for `N`
- [x] Implement `Drop` for proper cleanup of initialized items
- [x] Add `Send + Sync` bounds
- [x] Wire into `src/lib.rs` with `#[cfg(feature = "stack-ring")]`
- [x] Add SPSC tests in `tests/stack_ring_tests.rs`
- [x] Add type aliases (`StackRing4K`, `StackRing8K`, `StackRing16K`, `StackRing64K`)

### Phase 2: StackChannel ✅ COMPLETE
- [x] Create `src/stack_channel.rs` with `StackChannel<T, N, P>`
- [x] Implement `StackProducer<'a, T, N, P>` handle
- [x] Implement `register()`, `consume_all()`, `close()`
- [x] Add multi-producer tests in `tests/stack_channel_tests.rs`
- [x] Add stress tests with concurrent access
- [x] Add type aliases (`StackChannel4K4P`, `StackChannel4K8P`, etc.)

### Documentation & Polish ✅ COMPLETE
- [x] Add examples in `examples/stack_ring.rs`
- [x] Add benchmarks comparing heap vs stack variants
- [x] Add MPSC benchmarks comparing StackChannel vs Channel
- [x] Update README.md with feature flag usage

## Future Work

### StackSpanCollector for span_collector Example

The `examples/span_collector` uses the heap-based `Channel<Span>`. A stack-allocated variant could reduce span submission latency from ~100ns to ~30-50ns.

**Challenges:**

1. **Static producer count** — `StackChannel<T, N, P>` requires knowing `P` at compile time, but `SpanCollector` uses dynamic `register()`. Solution: Make `P` a const generic on `StackSpanCollector`.

2. **Lifetime management** — `StackProducer<'a, ...>` is tied to the channel's lifetime. For spawned async tasks, need `Box::leak` or scoped threads.

3. **`Span` is not `Copy`** — Contains `String` and `Box<HashMap>`, so can't use `send()` batch method. Must use `push()` or `reserve()`/`commit()` loops.

**Proposed API:**

```rust
// Dynamic producers, heap-allocated (existing)
let collector = SpanCollector::new(config);

// Fixed producers, stack-allocated (proposed)
let collector = StackSpanCollector::<4096, 8>::new();  // 4K ring, max 8 producers
```

**Implementation checklist:**

- [ ] Create `StackSpanCollector<const N: usize, const P: usize>`
- [ ] Wrap `StackChannel<Span, N, P>` internally
- [ ] Provide same API as `SpanCollector` (register, submit, consume)
- [ ] Add `StackSpanProducer` with `'static` trick for async compatibility
- [ ] Add benchmarks comparing heap vs stack span submission

## Benchmark Results

Benchmarks run on Apple M-series (ARM64), Rust 1.87, `--release` with default optimizations.

### SPSC: Stack vs Heap Comparison

| Benchmark | Heap `Ring<T>` | Stack `StackRing<T, N>` | Improvement |
|-----------|----------------|-------------------------|-------------|
| **Single Thread** (5M ops) | 1.47 Gelem/s | 1.48 Gelem/s | ~1.0x (parity) |
| **SPSC Cross-Thread** (5M ops) | 2.97 Gelem/s | **5.96 Gelem/s** | **2.0x faster** |

**Key finding**: StackRing achieves **2x throughput** in the cross-thread SPSC scenario — the most important workload for this library. The improvement comes from:
1. Eliminated pointer indirection (buffer is inline)
2. Better cache locality (contiguous memory layout)
3. Predictable memory addresses enable hardware prefetching

### MPSC: StackChannel vs Channel Comparison

| Producers | Heap `Channel<T>` | Stack `StackChannel<T,N,P>` | Improvement |
|-----------|-------------------|------------------------------|-------------|
| **2P** | 2.77 Gelem/s | **5.06 Gelem/s** | **1.8x faster** |
| **4P** | 2.65 Gelem/s | **6.01 Gelem/s** | **2.3x faster** |
| **8P** | 1.27 Gelem/s | **4.84 Gelem/s** | **3.8x faster** |

**Key finding**: StackChannel shows even larger improvements than SPSC, especially at higher producer counts. The **3.8x improvement at 8P** demonstrates the benefit of eliminating heap indirection when the consumer must poll multiple rings.

### Stack Ring Size Comparison

Testing different capacity const generics (all SPSC, 2M ops):

| Ring Size | Throughput | Notes |
|-----------|------------|-------|
| 4K slots (4096) | 1.34 Gelem/s | Fits L1 cache |
| 8K slots (8192) | 1.43 Gelem/s | ~L1/L2 boundary |
| 16K slots (16384) | 1.47 Gelem/s | Sweet spot for L2 |
| 64K slots (65536) | 1.46 Gelem/s | L2/L3 boundary |

The 16K-64K sizes perform best, likely hitting the L2 cache sweet spot where the working set fits but isn't too small to trigger frequent wrap-around.

### Running Benchmarks

```bash
cargo bench --features stack-ring --bench stack_vs_heap
```

View HTML report at `target/criterion/report/index.html`.

## Feature Flag Usage

Enable in `Cargo.toml`:

```toml
[dependencies]
ringmpsc-rs = { version = "0.1", features = ["stack-ring"] }
```

Or for development:

```bash
cargo build --features stack-ring
cargo test --features stack-ring
```

## References

- [Original rust_impl StackRing](https://github.com/debasishg/ringmpsc/blob/main/rust_impl/src/stack_ring.rs) — Source implementation
- [Zig Channel with comptime arrays](https://github.com/debasishg/ringmpsc/blob/main/src/channel.zig#L595) — Inspiration for StackChannel design
- [LMAX Disruptor](https://lmax-exchange.github.io/disruptor/) — Batch consumption pattern
