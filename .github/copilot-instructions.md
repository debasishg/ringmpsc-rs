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

### Reference Implementation
See [examples/span_collector/](examples/span_collector/) for a complete async tracing collector:
- Bridges ringmpsc → tokio via `tokio::sync::Notify` ([examples/span_collector/src/async_bridge.rs](examples/span_collector/src/async_bridge.rs))
- Demonstrates backpressure handling pattern ([examples/span_collector/docs/backpressure-notify-pattern.md](examples/span_collector/docs/backpressure-notify-pattern.md))
- Shows batch processing with `consume_all_up_to()` for bounded latency

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

## Testing Conventions

- **FIFO verification**: Use `(producer_id, sequence)` tuples, verify per-producer order ([tests/integration_tests.rs#L30](tests/integration_tests.rs#L30))
- **Stress tests**: 8+ producers × 50K+ items, verify sum equals expected ([tests/integration_tests.rs#L77](tests/integration_tests.rs#L77))
- **Loom tests**: Simplified ring in [tests/loom_tests.rs](tests/loom_tests.rs) for exhaustive interleaving
- **Miri tests**: [tests/miri_tests.rs](tests/miri_tests.rs) catches UB in unsafe paths

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
