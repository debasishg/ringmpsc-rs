# RingMPSC-RS Workspace Instructions

This file defines **global** conventions for the entire workspace. Crate-specific details belong in each crate's `spec.md`.

## Workspace Structure

```
ringmpsc-rs/
├── Cargo.toml                    # Workspace manifest
├── .github/copilot-instructions.md  # This file (global conventions)
├── crates/
│   ├── ringmpsc/                 # Lock-free ring buffer library
│   │   ├── Cargo.toml
│   │   ├── spec.md               # Ring buffer invariants
│   │   └── src/
│   └── span_collector/           # Async tracing collector (binary)
│       ├── Cargo.toml
│       ├── spec.md               # Collector invariants
│       └── src/
```

## Crate Specifications

Each crate has a `spec.md` documenting its invariants:
- [crates/ringmpsc/spec.md](../crates/ringmpsc/spec.md) - Memory layout, sequencing, ordering, drop safety
- [crates/span_collector/spec.md](../crates/span_collector/spec.md) - Domain model, ownership transfer, backpressure

## Global Coding Standards

### Error Handling

| Context | Rule |
|---------|------|
| Library code | **Never use `.unwrap()`** - return `Result` or `Option` |
| Test code | `.unwrap()` and `.expect()` are acceptable |
| Binary entry points | `.expect("descriptive message")` for unrecoverable failures |
| Panics | Only for logic errors that indicate bugs, never for recoverable conditions |

```rust
// ❌ BAD (library code)
let config = Config::new(bits).unwrap();

// ✅ GOOD (library code)
let config = Config::new(bits)?;

// ✅ GOOD (test code)
let config = Config::new(bits).expect("test config should be valid");
```

### Unsafe Code

1. **Minimize unsafe blocks** - wrap smallest possible scope
2. **Document invariants** - every `unsafe` block must have a `// Safety:` comment
3. **Validate with Miri** - run `cargo +nightly miri test` before merging

```rust
// ✅ GOOD
// Safety: idx is bounded by mask (power-of-2 capacity), and the slot
// at this index is initialized (head ≤ idx < tail per INV-INIT-01)
unsafe { buffer[idx].assume_init_read() }
```

### Memory Ordering

Use the minimum ordering required:

| Pattern | Ordering | Use Case |
|---------|----------|----------|
| Counters/metrics | `Relaxed` | No synchronization needed |
| Single-writer cache | `UnsafeCell` | Thread-local optimization |
| Publish data | `Release` | Writer publishes to readers |
| Read published data | `Acquire` | Reader synchronizes with writer |
| Read-modify-write | `AcqRel` | Concurrent updates (rare) |

### Performance

- **Release builds only** for lock-free code - debug builds break optimizations
- **Batch operations** preferred over single-item operations
- **No mutexes** in hot paths - use lock-free primitives
- **128-byte alignment** for hot atomics (prevent false sharing)

## Development Workflows

```bash
# Build entire workspace
cargo build --release

# Test specific crate
cargo test -p ringmpsc-rs
cargo test -p span_collector

# Concurrency testing (ringmpsc)
cargo test -p ringmpsc-rs --features loom --test loom_tests --release

# UB detection (ringmpsc)
cargo +nightly miri test -p ringmpsc-rs --test miri_tests

# Benchmarks (ringmpsc)
cargo bench -p ringmpsc-rs

# Run span_collector demo
cargo run -p span_collector --bin demo
```

## Test Organization

| Location | Purpose |
|----------|---------|
| `crates/*/src/**` | Unit tests (`#[cfg(test)]` modules) |
| `crates/*/tests/` | Integration tests |
| `crates/ringmpsc/tests/loom_tests.rs` | Exhaustive concurrency testing |
| `crates/ringmpsc/tests/miri_tests.rs` | UB detection |
| `crates/*/benches/` | Performance benchmarks (criterion) |

### Test Naming

- `test_*` - standard unit/integration tests
- `*_stress` - high-volume concurrency tests
- `*_fifo` - ordering verification tests

## Git Conventions

- Feature branches: `feature/description`
- Bug fixes: `fix/description`
- Commits: imperative mood ("Add feature" not "Added feature")
- PRs must pass: `cargo test --all`, `cargo clippy`, `cargo fmt --check`

## Adding New Crates

1. Create `crates/<name>/` with `Cargo.toml` and `src/lib.rs` (or `src/main.rs`)
2. Add to workspace members in root `Cargo.toml`
3. Create `crates/<name>/spec.md` documenting invariants
4. Use `workspace = true` for shared dependencies

## Common Gotchas

1. **Debug builds break lock-free code** - always use `--release` for correctness testing
2. **Reservation wrap-around** - `reserve(n)` may return `len() < n`, must loop
3. **`MaybeUninit` writes** - use `slot.write(value)` not assignment
4. **Backpressure** - ring full → caller must handle retry logic
