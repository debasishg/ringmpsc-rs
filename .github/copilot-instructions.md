# RingMPSC-RS Workspace Instructions

Lock-free MPSC channel achieving **6+ billion msg/sec** via ring decomposition (each producer gets a dedicated SPSC ring).

## Architecture

| Crate | Purpose | Spec |
|-------|---------|------|
| `ringmpsc` | Core lock-free SPSC rings + MPSC channel | [spec.md](../crates/ringmpsc/spec.md) |
| `ringmpsc-stream` | Async `Stream`/`Sink` adapters with backpressure | [spec.md](../crates/ringmpsc-stream/spec.md) |
| `span_collector` | OpenTelemetry collector example (async batching, resilience) | [spec.md](../crates/span_collector/spec.md) |

**Key design**: Ring decomposition eliminates producer-producer contention. Each `Producer` owns one `Ring<T>`. Channel polls all rings sequentially on single consumer thread.

## Critical Patterns

### Invariants Module Pattern
Each crate has `src/invariants.rs` with `debug_assert!` macros referencing spec IDs:
```rust
// In invariants.rs
macro_rules! debug_assert_bounded_count {
    ($count:expr, $capacity:expr) => {
        debug_assert!($count <= $capacity, "INV-SEQ-01 violated: count {} > capacity {}", $count, $capacity)
    };
}
// Usage in ring.rs:
debug_assert_bounded_count!(count, self.capacity());
```

### Unsafe Code - Always Document
Every `unsafe` block requires `// Safety:` comment explaining why invariants hold:
```rust
// Safety: idx bounded by mask, slot initialized (head ≤ idx < tail per INV-INIT-01)
unsafe { buffer[idx].assume_init_read() }
```

### Memory Ordering (Minimum Required)
| Pattern | Ordering | Example |
|---------|----------|---------|
| Metrics | `Relaxed` | `spans_submitted.fetch_add(1, Relaxed)` |
| Single-writer cache | `UnsafeCell` | `cached_head` (producer-only) |
| Publish data | `Release` | `tail.store(new_tail, Release)` |
| Read published | `Acquire` | `tail.load(Acquire)` |

### Reservation API (Zero-Copy)
**Critical**: `reserve(n)` may return `len() < n` due to wrap-around. Always loop:
```rust
while remaining > 0 {
    if let Some(mut r) = ring.reserve(remaining) {
        remaining -= r.len();  // MAY BE < remaining
        r.as_mut_slice()[0] = MaybeUninit::new(item);
        r.commit();
    }
}
```

### Async Traits (span_collector pattern)
Native async traits (`impl Future`) aren't object-safe. Use boxed trait pattern:
```rust
trait SpanExporter { fn export(&self, batch: SpanBatch) -> impl Future<...> + Send; }
trait SpanExporterBoxed { fn export_boxed(&self, batch: SpanBatch) -> Pin<Box<dyn Future<...>>>; }
impl<T: SpanExporter> SpanExporterBoxed for T { ... }  // Blanket impl
```

## Development Commands

```bash
# Full workspace
cargo build --release && cargo test --workspace --release

# Crate-specific
cargo test -p ringmpsc-rs --features stack-ring --release
cargo test -p ringmpsc-stream --release
cargo test -p span_collector --release

# Concurrency verification (ringmpsc only)
cargo test -p ringmpsc-rs --features loom --test loom_tests --release
cargo +nightly miri test -p ringmpsc-rs --test miri_tests

# Benchmarks
cargo bench -p ringmpsc-rs
cargo bench -p ringmpsc-rs --features stack-ring --bench stack_vs_heap
```

### Concurrency Testing Requirements
- **Run loom tests** whenever changing atomic operations, memory ordering, or synchronization logic
- **Run miri tests** when modifying `unsafe` code or `MaybeUninit` handling
- Both loom and miri tests are **mandatory in CI/CD** before merge

### TLA+ Model Checking (ringmpsc only)
Formal specs in `crates/ringmpsc/tla/` verify lock-free invariants:
```bash
cd crates/ringmpsc/tla && tlc RingSPSC.tla -config RingSPSC.cfg -workers auto
```

## Error Handling Rules

| Context | Rule |
|---------|------|
| Library code | Return `Result`/`Option`, never `.unwrap()` |
| Tests | `.expect("reason")` acceptable |
| Binary entry | `.expect("fatal: reason")` for unrecoverable |

## Adding New Crates

1. Create `crates/<name>/` with `Cargo.toml`, `src/lib.rs`
2. Add to `[workspace.members]` in root `Cargo.toml`
3. **Create `spec.md`** documenting invariants with `INV-*` naming
4. **Create `src/invariants.rs`** with `debug_assert!` macros per spec
5. Use `workspace = true` for shared deps

## Gotchas

- **Debug builds break lock-free code** - always `--release` for correctness
- **128-byte alignment** required for hot atomics (cache line = 64B, Intel prefetches 2)
- **`MaybeUninit` writes** - use `slot.write(value)` not `*slot = value`
- **Backpressure** - ring full returns `None`, caller must retry with backoff
- **Stack rings** (`StackRing<T, N>`) are 2-3× faster but require compile-time capacity
