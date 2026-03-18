# RingMPSC-RS

A Rust workspace of high-performance, lock-free data structures built around **ring decomposition** — the idea that each producer gets a dedicated SPSC ring buffer, eliminating producer-producer contention entirely.

Originally a port of the [RingMPSC](https://github.com/boonzy00/ringmpsc) Zig implementation, the project has grown into a family of crates that apply the same principle to async streams, write-ahead logs, and distributed tracing.

## Workspace Crates

```
ringmpsc-rs/
└── crates/
    ├── ringmpsc/          Core lock-free SPSC ring + MPSC channel
    ├── ringmpsc-stream/   Async Stream/Sink adapters (depends on ringmpsc)
    ├── ringwal/           Write-Ahead Log engine  (depends on ringmpsc, ringmpsc-stream)
    ├── ringwal-store/     Storage backend trait + recovery-to-store bridge (depends on ringwal)
    ├── ringwal-sim/       Deterministic simulation testing for ringwal (depends on ringwal)
    └── span_collector/    OpenTelemetry span collector example (depends on ringmpsc)
```

### Dependency Graph

```
ringmpsc  ◄── ringmpsc-stream ◄── ringwal ◄── ringwal-store
    ▲                              ▲
    └──────── span_collector       └── ringwal-sim
```

| Crate | Description | Docs |
|-------|-------------|------|
| [ringmpsc](crates/ringmpsc/) | Lock-free SPSC rings and MPSC channel with zero-copy reservation API. Heap and stack-allocated variants. | [README](crates/ringmpsc/README.md) · [spec](crates/ringmpsc/spec.md) |
| [ringmpsc-stream](crates/ringmpsc-stream/) | `futures::Stream` / `futures::Sink` adapters with backpressure, hybrid polling, and graceful shutdown. | [README](crates/ringmpsc-stream/README.md) · [spec](crates/ringmpsc-stream/spec.md) |
| [ringwal](crates/ringwal/) | Write-Ahead Log backed by per-writer SPSC rings. Group commit, segment rotation, CRC32 checksums, crash recovery. | [README](crates/ringwal/README.md) · [spec](crates/ringwal/spec.md) |
| [ringwal-store](crates/ringwal-store/) | Storage backend trait (`WalStore`) and in-memory reference implementation. Bridges WAL recovery to application state. | [spec](crates/ringwal-store/spec.md) |
| [span_collector](crates/span_collector/) | Async OpenTelemetry-compatible span collector with batching, retry, circuit breaker, and rate limiting. | [README](crates/span_collector/README.md) · [spec](crates/span_collector/spec.md) |

## Architecture

The core insight is **ring decomposition**: instead of a single shared queue, each producer owns a dedicated SPSC ring buffer.

```
Producer 1 ──→ [Ring 1] ──┐
Producer 2 ──→ [Ring 2] ──┼──→ Consumer (polls all rings)
Producer 3 ──→ [Ring 3] ──┘
```

- No producer-producer contention (each ring is single-writer)
- Consumer polls all rings sequentially on a single thread
- Per-producer FIFO ordering; no global ordering across producers
- Unbounded `u64` sequences prevent ABA (58 years to wrap at 10B msg/sec)
- 128-byte cache alignment for hot atomics (prevents false sharing)
- Batch consumption — single atomic update for N items (Disruptor pattern)
- Zero-copy reservation API — `reserve()` → write → `commit()`

## Performance

Benchmarked on Apple M2 (release build, `ringmpsc` crate):

| Configuration | Throughput |
|--------------|------------|
| SPSC (heap) | 2.97 B msg/s |
| SPSC (stack) | **5.94 B msg/s** |
| MPSC 4P (heap) | 2.73 B msg/s |
| MPSC 4P (stack) | **6.71 B msg/s** |
| MPSC 8P (stack) | 4.76 B msg/s |

Stack-allocated rings (`StackRing<T, N>`) achieve **2-3× better throughput** due to improved cache locality.

## Quick Start

```bash
# Build the entire workspace in release mode
cargo build --release

# Run all tests across all crates
cargo test --workspace --release

# Run tests for a specific crate
cargo test -p ringmpsc-rs --release
cargo test -p ringmpsc-stream --release
cargo test -p ringwal --release
cargo test -p span_collector --release
```

See each crate's README for detailed build, test, and usage instructions.

## Formal Verification

The SPSC ring buffer protocol is formally specified in both TLA+ and [Quint](https://quint-lang.org/), with model-based testing bridging the spec to the Rust implementation via [`quint-connect`](https://crates.io/crates/quint-connect).

```bash
cd crates/ringmpsc/tla

# Fast simulation with invariant checking (Rust backend, default since Quint 0.31)
quint run RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant

# Exhaustive model checking via TLC (requires JDK 21+)
quint verify RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant --backend=tlc

# Model-based testing — replay spec traces against real Ring<T>
cargo test -p ringmpsc-rs --test quint_mbt --features quint-mbt --release
```

See [FORMAL_VERIFICATION_WORKFLOW.md](docs/FORMAL_VERIFICATION_WORKFLOW.md) and [tla/README.md](crates/ringmpsc/tla/README.md) for details.

## License

MIT

## Acknowledgments

- Original [RingMPSC Zig implementation](https://github.com/boonzy00/ringmpsc) by boonzy00
- Inspired by the [LMAX Disruptor](https://lmax-exchange.github.io/disruptor/) pattern
