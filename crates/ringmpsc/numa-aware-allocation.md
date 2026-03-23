# NUMA-Aware Ring Allocation

> **Status**: Implementation in progress  
> **Last updated**: 2026-03-23

## Summary

Add a `NumaAllocator` to ringmpsc that binds each ring buffer's backing memory to a specific NUMA node via `libc::mbind` on Linux, with graceful fallback to `HeapAllocator` on non-NUMA platforms (macOS, Windows). A `NumaPolicy` enum (`Fixed(node)`, `RoundRobin`, `ProducerLocal`) controls how nodes are assigned per-ring inside a `Channel`. The allocator plugs into the existing `BufferAllocator` trait — zero changes to the lock-free core.

## Background: Why NUMA Matters for Ring Decomposition

In ringmpsc's architecture, each producer owns a dedicated SPSC ring. On a 2-socket (or more) server:

- **Without NUMA awareness**: All ring buffers land on whatever node the OS chooses (typically node 0). Producers on the remote socket pay ~100ns cross-interconnect (QPI/UPI) penalty per cache-line access.
- **With NUMA awareness**: Each ring buffer is allocated on the NUMA node where its producer thread runs. All writes hit local DRAM, cutting latency by 2-3× and eliminating cross-socket traffic.

```text
┌─────────────── Socket 0 ────────────────┐  ┌─────────────── Socket 1 ────────────────┐
│  Core 0 ──► Ring[0] (NUMA node 0)       │  │  Core 8 ──► Ring[2] (NUMA node 1)       │
│  Core 1 ──► Ring[1] (NUMA node 0)       │  │  Core 9 ──► Ring[3] (NUMA node 1)       │
│                                          │  │                                          │
│       Consumer thread (reads all)        │  │                                          │
└──────────────────────────────────────────┘  └──────────────────────────────────────────┘
                                    ▲
                            QPI / UPI link (~100ns)
```

The existing `BufferAllocator` trait (`Ring<T, A>`, `Channel<T, A>`) is the perfect extension point — `NumaAllocator` implements `BufferAllocator`, and the core ring code is unchanged.

## Implementation Plan

### Phase 1: Core NumaAllocator (Linux FFI + fallback)

#### 1. Add `numa` feature flag

**File**: `crates/ringmpsc/Cargo.toml`

Add `libc` as an optional dependency. Gate all NUMA code behind `#[cfg(feature = "numa")]`. No external `libnuma` crate — use raw `libc::mmap` + `libc::mbind` syscalls to keep dependencies minimal.

```toml
[features]
numa = ["dep:libc"]

[dependencies]
libc = { version = "0.2", optional = true }
```

#### 2. Create `src/numa.rs` module

**File**: `crates/ringmpsc/src/numa.rs` (new)

Define the public API:

```rust
/// NUMA memory placement policy for ring buffer allocation.
pub enum NumaPolicy {
    /// All rings allocated on a single, specified NUMA node.
    Fixed(u16),
    /// Distribute rings across NUMA nodes in round-robin order.
    RoundRobin,
    /// Allocate on the NUMA node local to the calling thread's CPU.
    /// **Caveat**: With `Channel`, rings are pre-allocated at construction
    /// time, so `ProducerLocal` resolves to the *channel creator's* node.
    /// For true per-producer placement, use `Ring::new_in()` from each
    /// producer thread instead.
    ProducerLocal,
}

/// NUMA-aware ring buffer allocator.
///
/// Binds ring buffer memory to specific NUMA nodes via `libc::mbind`
/// on Linux. Falls back to `HeapAllocator` on non-Linux platforms.
pub struct NumaAllocator { ... }
```

Internal components:

| Component | Purpose |
|-----------|---------|
| `NumaPolicy` | Controls node selection per `allocate()` call |
| `NumaAllocator` | Holds policy + `Arc<AtomicU16>` counter for RoundRobin |
| `NumaBuffer<T>` | Owns `mmap`'d memory, `munmap` on Drop |
| `NumaInfo` | Cached topology: `num_nodes`, `available_nodes` |
| `numa_available()` | Platform detection helper |

#### 3. Implement `BufferAllocator` for `NumaAllocator`

The `allocate<T>()` method:

**Linux path** (`cfg(target_os = "linux")`):

1. Resolve target node from policy:
   - `Fixed(n)` → use node `n`
   - `RoundRobin` → `counter.fetch_add(1) % num_nodes`
   - `ProducerLocal` → `libc::sched_getcpu()` → `get_mempolicy()` for current node
2. `libc::mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0)`
3. `libc::mbind(ptr, len, MPOL_BIND, nodemask, maxnode, 0)` to pin pages to node
4. Wrap in `NumaBuffer<T>`

**Non-Linux path** (`cfg(not(target_os = "linux"))`):

Delegate to `HeapAllocator::allocate()`. Emit a one-time `eprintln!` warning that NUMA is not available.

#### 4. Define `NumaBuffer<T>`

Modeled after `AlignedBuffer<T, ALIGN>` in `src/allocator.rs`:

```rust
pub struct NumaBuffer<T> {
    ptr: *mut MaybeUninit<T>,  // Typed pointer into mmap region
    len: usize,                // Number of T elements
    mmap_ptr: *mut u8,         // Raw mmap base (for munmap)
    mmap_len: usize,           // Total mmap size (for munmap)
}

impl<T> Drop for NumaBuffer<T> {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.mmap_ptr as *mut libc::c_void, self.mmap_len); }
    }
}
```

- `Deref` / `DerefMut` → `&[MaybeUninit<T>]` (same pattern as `AlignedBuffer`)
- `unsafe impl Send` — buffer owns its allocation exclusively

#### 5. Clone semantics for Channel use

`Channel::new_in()` requires `A: BufferAllocator + Clone`. For `NumaAllocator`:

- `Fixed` / `RoundRobin`: clone shares the `Arc<AtomicU16>` counter, so each `allocate()` call on any clone advances the same counter. This distributes rings across nodes automatically.
- `ProducerLocal`: clone is trivial — node resolved at allocation time (stateless).

### Phase 2: Spec & Invariants

#### 6. Add spec invariants to `spec.md`

| Invariant | Requirement |
|-----------|-------------|
| **INV-NUMA-01** (Memory Placement) | `NumaAllocator::allocate()` must place memory on the requested NUMA node. Verified via `/proc/self/numa_maps` in tests. |
| **INV-NUMA-02** (Fallback Safety) | On non-NUMA platforms, `NumaAllocator` must produce a valid buffer satisfying INV-MEM-04. |
| **INV-NUMA-03** (Policy Determinism) | `RoundRobin` assigns nodes in strictly increasing cyclic order across sequential `allocate()` calls. |

#### 7. Add debug assertion macro to `invariants.rs`

```rust
macro_rules! debug_assert_numa_placement {
    ($ptr:expr, $node:expr) => { ... };
}
```

On Linux, reads `/proc/self/numa_maps` to verify page placement. No-op on other platforms.

### Phase 3: Channel Integration

#### 8. No changes to `Channel::new_in()`

It already clones the allocator per ring. With `RoundRobin`, each `alloc.clone()` produces a clone sharing the counter, and each `alloc.allocate()` call advances the counter — rings distribute across nodes automatically.

#### 9. Add convenience constructor

```rust
impl<T> Channel<T, NumaAllocator> {
    pub fn new_numa(config: Config, policy: NumaPolicy) -> Self {
        Self::new_in(config, NumaAllocator::new(policy))
    }
}
```

### Phase 4: Testing

#### 10. Create `tests/numa_tests.rs`

Feature-gated `#[cfg(feature = "numa")]`, following the 4-pattern structure from `tests/allocator_tests.rs`:

| Test | Description |
|------|-------------|
| `test_numa_allocator_fixed_node` | Allocate buffer, verify via `/proc/self/numa_maps` (Linux CI only) |
| `test_numa_allocator_round_robin` | Allocate N buffers, verify node distribution |
| `test_numa_allocator_producer_local` | Spawn thread, allocate, verify local node |
| `test_numa_allocator_fallback` | On non-Linux, allocator produces valid buffer (INV-NUMA-02) |
| `test_numa_channel_integration` | Channel with RoundRobin: register, push, consume |
| `test_numa_buffer_drop` | Verify `munmap` called, no leaks |
| `test_numa_ring_push_consume` | Basic Ring operations with NumaAllocator |
| `test_numa_ring_reserve_commit` | Zero-copy API with NumaAllocator |

### Phase 5: Documentation & README

#### 11. Update README.md

Check off the NUMA TODO item. Add feature flag table entry and usage example:

```rust
use ringmpsc_rs::{Channel, Config, NumaAllocator, NumaPolicy};

// Round-robin: distribute ring buffers across NUMA nodes
let channel = Channel::<u64, NumaAllocator>::new_in(
    Config::default(),
    NumaAllocator::new(NumaPolicy::RoundRobin),
);
```

#### 12. Update `docs/CUSTOM_ALLOCATORS.md`

Add `NumaAllocator` section alongside HeapAllocator, AlignedAllocator, StdAllocator.

#### 13. Update `PERFORMANCE.md`

Add expected latency improvements for cross-socket scenarios.

## Relevant Files

| File | Change |
|------|--------|
| `crates/ringmpsc/Cargo.toml` | Add `numa` feature + `libc` optional dep |
| `crates/ringmpsc/src/numa.rs` | **New**: `NumaAllocator`, `NumaPolicy`, `NumaBuffer`, FFI |
| `crates/ringmpsc/src/lib.rs` | Conditionally `pub mod numa;` + re-exports |
| `crates/ringmpsc/src/invariants.rs` | Add `debug_assert_numa_placement!` |
| `crates/ringmpsc/src/channel.rs` | Add `Channel::new_numa()` convenience |
| `crates/ringmpsc/spec.md` | Add INV-NUMA-01/02/03 |
| `crates/ringmpsc/tests/numa_tests.rs` | **New**: NUMA-specific tests |
| `crates/ringmpsc/README.md` | Check off TODO, add usage example |
| `docs/CUSTOM_ALLOCATORS.md` | Add NumaAllocator section |

## Verification

```bash
# NUMA tests pass
cargo test -p ringmpsc-rs --features numa --release

# Existing tests unaffected (NUMA feature-gated)
cargo test -p ringmpsc-rs --release

# macOS build succeeds (fallback path)
cargo build -p ringmpsc-rs --features numa

# Loom tests still pass (no atomics changed)
cargo test -p ringmpsc-rs --features loom --test loom_tests --release
```

On a Linux NUMA machine: verify `/proc/self/numa_maps` shows correct node binding. Verify `NumaBuffer` drop doesn't leak via valgrind/ASan.

## Design Decisions

| Decision | Rationale |
|----------|-----------|
| **Cross-platform with fallback** | Linux uses `libc::mbind`; macOS/Windows fall back to `HeapAllocator`. Feature-gated behind `numa`. |
| **Policy-based API** | `NumaPolicy::{Fixed, RoundRobin, ProducerLocal}` covers all multi-socket topologies. |
| **Raw `libc` FFI, no `libnuma` crate** | Avoids C library dependency; `mmap` + `mbind` are stable Linux syscalls. |
| **No changes to lock-free core** | `BufferAllocator` trait absorbs all NUMA complexity. |
| **Allocator not stored in ring** | Follows existing pattern — allocator consumed at construction; only `NumaBuffer` survives. |

## Caveats

### `ProducerLocal` with `Channel`

Since `Channel::new_in()` pre-allocates all rings eagerly, `ProducerLocal` resolves to the *channel creator's* node, not each producer's node. For true per-producer NUMA placement:

1. Use individual `Ring::new_in(config, NumaAllocator::new(NumaPolicy::ProducerLocal))` from each producer thread, or
2. Use `NumaPolicy::Fixed(node)` / `RoundRobin` with `Channel`, or
3. Wait for a future lazy-allocation mode (separate PR).

### Huge Pages + NUMA

Users wanting both 2MiB TLB optimization and NUMA binding can set `huge_pages: true` on `NumaAllocator`, which adds `MAP_HUGETLB` to the `mmap` flags. This is simpler than composing `AlignedAllocator` with `NumaAllocator`.

### CI Testing

NUMA placement tests require a multi-node machine. GitHub Actions runners are single-socket. Tests that verify actual NUMA placement should be `#[ignore]`'d by default and run manually on multi-socket hardware.
