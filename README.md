# RingMPSC-RS

A high-performance lock-free Multi-Producer Single-Consumer (MPSC) channel implementation in Rust, achieving **6+ billion messages per second**.

This is a Rust port of the [RingMPSC](https://github.com/boonzy00/ringmpsc) Zig implementation, using **ring decomposition**: each producer gets a dedicated SPSC ring buffer, eliminating producer-producer contention.

## Workspace Structure

```
ringmpsc-rs/
├── crates/
│   ├── ringmpsc/          # Core lock-free ring buffer library
│   └── span_collector/    # Async OpenTelemetry tracing collector (example app)
```

| Crate | Description | Documentation |
|-------|-------------|---------------|
| [ringmpsc](crates/ringmpsc/) | Lock-free MPSC channel with zero-copy API | [README](crates/ringmpsc/README.md) · [spec.md](crates/ringmpsc/spec.md) |
| [span_collector](crates/span_collector/) | Async tracing collector built on ringmpsc | [README](crates/span_collector/README.md) · [spec.md](crates/span_collector/spec.md) |

## Performance

Benchmarked on Apple M2 (release build):

| Configuration | Throughput |
|--------------|------------|
| SPSC (heap) | 2.97 B/s |
| SPSC (stack) | **5.94 B/s** |
| MPSC 4P (heap) | 2.73 B/s |
| MPSC 4P (stack) | **6.71 B/s** |
| MPSC 8P (stack) | 4.76 B/s |

Stack-allocated rings (`StackRing<T, N>`) achieve **2-3× better throughput** due to improved cache locality.

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
ringmpsc-rs = { git = "https://github.com/debasishg/ringmpsc-rs" }
```

### Basic Usage

```rust
use ringmpsc_rs::{Channel, Config};

// Create channel (64K slots, up to 16 producers)
let channel = Channel::<u64>::new(Config::default());

// Register a producer
let producer = channel.register().unwrap();

// Zero-copy send
if let Some(mut reservation) = producer.reserve(1) {
    reservation.as_mut_slice()[0] = 42;
    reservation.commit();
}

// Batch consume
channel.consume_all(|item: &u64| {
    println!("Received: {}", item);
});
```

### Stack-Allocated Ring (Higher Performance)

Enable the `stack-ring` feature:

```toml
[dependencies]
ringmpsc-rs = { git = "https://github.com/debasishg/ringmpsc-rs", features = ["stack-ring"] }
```

```rust
use ringmpsc_rs::{StackChannel, StackProducer};

// 64K slots, up to 4 producers, all stack-allocated
let channel = StackChannel::<u64, 65536, 4>::new();
let producer = channel.register().unwrap();

producer.push(42);
```

## Development

```bash
# Build workspace
cargo build --release

# Run all tests
cargo test --workspace --release

# Run tests with stack-ring feature
cargo test -p ringmpsc-rs --features stack-ring --release

# Run benchmarks
cargo bench -p ringmpsc-rs
cargo bench -p ringmpsc-rs --features stack-ring --bench stack_vs_heap

# Run standalone benchmark binaries
cargo run -p ringmpsc-rs --release --bin scaling_benchmark
cargo run -p ringmpsc-rs --release --bin bench_final

# Concurrency testing (loom)
cargo test -p ringmpsc-rs --features loom --test loom_tests --release

# UB detection (miri)
cargo +nightly miri test -p ringmpsc-rs --test miri_tests

# Run span_collector demo
cargo run -p span_collector --release --bin demo
```

## Features

| Feature | Description |
|---------|-------------|
| `stack-ring` | Stack-allocated `StackRing<T, N>` and `StackChannel<T, N, P>` |
| `loom` | Enable loom for exhaustive concurrency testing |

## Architecture

The library uses **ring decomposition** to achieve lock-free MPSC:

```
Producer 1 ──→ [Ring 1] ──┐
Producer 2 ──→ [Ring 2] ──┼──→ Consumer (polls all rings)
Producer 3 ──→ [Ring 3] ──┘
```

- Each producer gets a dedicated SPSC ring (no producer contention)
- Consumer polls all rings sequentially (single-threaded)
- Per-producer FIFO ordering (no global ordering across producers)

### Key Design Decisions

- **Unbounded u64 sequences** prevent ABA problem (58 years to wrap at 10B msg/sec)
- **128-byte cache alignment** for hot atomics (prevents false sharing)
- **Batch consumption** - single atomic update for N items (Disruptor pattern)
- **Zero-copy API** - `reserve()` → write → `commit()` avoids intermediate copies

## Formal Verification

The SPSC ring buffer protocol is formally specified in both TLA+ and [Quint](https://quint-lang.org/), with model-based testing bridging the spec to the real implementation via [`quint-connect`](https://crates.io/crates/quint-connect).

### Quint 0.31.0 Improvements

Since [Quint 0.31.0](https://github.com/informalsystems/quint/releases/tag/v0.31.0) (2026-02-27), the verification workflow has been significantly streamlined:

| Capability | What changed | Impact |
|------------|-------------|--------|
| **Rust backend is default** | `quint run` / `quint test` now use the Rust backend | ~10× faster simulation and MBT trace generation |
| **`quint verify --backend=tlc`** | TLC model checking directly from `.qnt` files | `.qnt` becomes single source of truth — no need to maintain a separate `.tla` file |
| **`--invariant` in simulation** | `quint run --invariant=safetyInvariant` | Quick invariant checking across thousands of random traces |
| **`--mbt` in Rust backend** | `quint run --mbt` natively supported | Faster trace generation for `quint-connect` model-based tests |

```bash
cd crates/ringmpsc/tla

# Fast simulation with invariant checking (Rust backend, default)
quint run RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant

# Exhaustive model checking via TLC (955 states, requires JDK 21+)
quint verify RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant --backend=tlc

# Symbolic model checking via Apalache (complementary, requires JDK 21+)
quint verify RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant

# Run embedded spec tests
quint test RingSPSC.qnt --main=RingSPSC

# Model-based testing — replay spec traces against real Ring<T>
cargo test -p ringmpsc-rs --test quint_mbt --features quint-mbt --release
```

### Next Steps

- ~~**Upgrade JDK to 17+**~~ Done: JDK 21 LTS installed; both `quint verify` backends operational
- **Retain `RingSPSC.tla` for liveness** — `EventuallyConsumed` temporal property (`~>`) has no Quint equivalent; `.tla` kept for liveness, `.qnt` is single source of truth for safety
- **Add `q::debug` diagnostics** to the Quint spec for richer per-step tracing during MBT
- **Add CI workflow** for `quint verify` (both backends) + MBT tests (`.github/workflows/quint.yml`)

See [FORMAL_VERIFICATION_WORKFLOW.md](docs/FORMAL_VERIFICATION_WORKFLOW.md) and [tla/README.md](crates/ringmpsc/tla/README.md) for details.

## License

MIT

## Acknowledgments

- Original [RingMPSC Zig implementation](https://github.com/boonzy00/ringmpsc) by boonzy00
- Inspired by the [LMAX Disruptor](https://lmax-exchange.github.io/disruptor/) pattern
