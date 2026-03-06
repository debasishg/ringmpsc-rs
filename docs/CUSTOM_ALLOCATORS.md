# Custom Allocators in RingMPSC

> **Last updated**: 2026-03-06

This document describes the custom allocator subsystem in `ringmpsc`, which allows the ring buffer's backing memory to be provided by any allocation strategy — heap, cache-line-aligned, arena, huge-page, NUMA-local, or anything else — without modifying the lock-free core.

## Table of Contents

- [Motivation](#motivation)
- [Design Overview](#design-overview)
- [The `BufferAllocator` Trait](#the-bufferallocator-trait)
- [Built-in Allocators](#built-in-allocators)
  - [`HeapAllocator` (default)](#heapallocator-default)
  - [`AlignedAllocator<ALIGN>`](#alignedallocatoralign)
  - [`StdAllocator<A>` (nightly)](#stdallocatora-nightly)
- [Using Custom Allocators](#using-custom-allocators)
  - [With `Ring<T, A>`](#with-ringt-a)
  - [With `Channel<T, A>`](#with-channelt-a)
- [Implementing Your Own Allocator](#implementing-your-own-allocator)
- [Invariants and Safety](#invariants-and-safety)
- [How Alignment Works Internally](#how-alignment-works-internally)
- [Performance Considerations](#performance-considerations)
- [Testing](#testing)
- [Formal Verification (Quint Model)](#formal-verification-quint-model)
- [Code Map](#code-map)

---

## Motivation

Lock-free ring buffers spend nearly all their time accessing the backing buffer — reading and writing `MaybeUninit<T>` slots at indices derived from head/tail sequence numbers. The allocation strategy for that buffer has a direct impact on:

1. **False sharing** — If the buffer start isn't cache-line aligned, producer and consumer may contend on the same cache line.
2. **TLB pressure** — Huge-page-backed allocations (2 MiB alignment) reduce TLB misses for large rings.
3. **NUMA locality** — On multi-socket systems, binding the buffer to a specific NUMA node keeps traffic local.
4. **Arena/pool reuse** — In latency-sensitive systems, pre-allocating from an arena avoids `malloc` jitter.

The allocator subsystem makes all of these strategies pluggable via a single generic parameter `A` on `Ring<T, A>` and `Channel<T, A>`, while guaranteeing **zero overhead** for the default heap path.

## Design Overview

The architecture follows Rust's "zero-cost abstraction" principle:

```text
┌──────────────────────────────────────────────────┐
│  Ring<T, A: BufferAllocator = HeapAllocator>     │
│                                                  │
│  buffer: UnsafeCell<A::Buffer<T>>                │
│          ▲                                       │
│          │  Deref → &[MaybeUninit<T>]            │
│          │  DerefMut → &mut [MaybeUninit<T>]     │
│                                                  │
│  Constructed via: Ring::new_in(config, alloc)    │
│  The allocator is consumed at construction,      │
│  NOT stored in the ring.                         │
└──────────────────────────────────────────────────┘
```

Key design decisions:

- **Generic parameter with default** — `Ring<T>` is sugar for `Ring<T, HeapAllocator>`, preserving backward compatibility.
- **Allocator not stored** — The allocator is used once at construction to produce a `Buffer<T>`. The buffer owns its memory and handles deallocation via `Drop`. This avoids storing an extra field.
- **`unsafe trait`** — The trait is `unsafe` because the ring's lock-free hot path assumes the buffer's `Deref`/`DerefMut` slices are valid and contiguous. A broken allocator causes UB.

## The `BufferAllocator` Trait

Defined in [crates/ringmpsc/src/allocator.rs](../crates/ringmpsc/src/allocator.rs):

```rust
pub unsafe trait BufferAllocator: Send + Sync {
    /// The owned buffer type. Must deref to a contiguous MaybeUninit<T> slice.
    type Buffer<T>: Deref<Target = [MaybeUninit<T>]> + DerefMut;

    /// Allocate a buffer of `capacity` uninitialized elements.
    fn allocate<T>(&self, capacity: usize) -> Self::Buffer<T>;
}
```

The `unsafe` keyword places the burden of proof on the implementor. The four safety requirements are codified as **INV-MEM-04** in [spec.md](../crates/ringmpsc/spec.md):

1. `allocate(capacity)` returns a buffer of exactly `capacity` elements.
2. The memory is valid for reads and writes for the buffer's lifetime.
3. The buffer's `Deref`/`DerefMut` targets are contiguous slices.
4. The buffer's `Drop` correctly deallocates the memory.

## Built-in Allocators

### `HeapAllocator` (default)

A zero-sized type that allocates via `Vec::into_boxed_slice()` — identical to what the ring buffer did before the allocator subsystem existed.

```rust
#[derive(Clone, Copy, Debug, Default)]
pub struct HeapAllocator;

unsafe impl BufferAllocator for HeapAllocator {
    type Buffer<T> = Box<[MaybeUninit<T>]>;

    fn allocate<T>(&self, capacity: usize) -> Box<[MaybeUninit<T>]> {
        let mut buffer = Vec::with_capacity(capacity);
        buffer.resize_with(capacity, MaybeUninit::uninit);
        buffer.into_boxed_slice()
    }
}
```

**Zero-overhead guarantee (INV-ALLOC-02):** `HeapAllocator` is a ZST, verified at compile time:

```rust
const _: () = assert!(
    std::mem::size_of::<HeapAllocator>() == 0,
    "INV-ALLOC-02 violated: HeapAllocator must be a zero-sized type"
);
```

This means `Ring<T>` and `Ring<T, HeapAllocator>` have identical struct size and machine code. The compiler monomorphizes both to the same binary output — no vtable, no indirection.

### `AlignedAllocator<ALIGN>`

Produces allocations aligned to any power-of-two boundary. Useful for cache-line (64/128 byte) or huge-page (2 MiB) alignment.

```rust
use ringmpsc_rs::{AlignedAllocator, Config, Ring};

// 128-byte aligned — eliminates false sharing (two cache lines on Intel)
let ring = Ring::<u64, AlignedAllocator<128>>::new_in(
    Config::default(),
    AlignedAllocator::<128>,
);
ring.push(42);
```

Internally, `AlignedAllocator` over-allocates a `Vec<u8>` with `ALIGN - 1` extra bytes, then computes an aligned pointer within the allocation. See [How Alignment Works Internally](#how-alignment-works-internally) for details.

### `StdAllocator<A>` (nightly)

Behind the `allocator-api` feature flag (requires nightly Rust), `StdAllocator` bridges any `std::alloc::Allocator` to `BufferAllocator`:

```rust
#![feature(allocator_api)]
use ringmpsc_rs::{Config, Ring, StdAllocator};
use std::alloc::Global;

let ring = Ring::new_in(Config::default(), StdAllocator(Global));
```

This enables integration with nightly allocator implementations such as `jemalloc`, `mimalloc`, or custom `Allocator` impls.

## Using Custom Allocators

### With `Ring<T, A>`

```rust
use ringmpsc_rs::{AlignedAllocator, Config, Ring};

// Ring::new() uses HeapAllocator (unchanged API)
let ring = Ring::<u64>::new(Config::default());

// Ring::new_in() accepts any BufferAllocator
let aligned_ring = Ring::<u64, AlignedAllocator<128>>::new_in(
    Config::default(),
    AlignedAllocator::<128>,
);

// All ring operations work identically regardless of allocator
aligned_ring.push(42);
aligned_ring.consume_batch(|val| println!("{}", val));
```

### With `Channel<T, A>`

The MPSC channel propagates the allocator to all its internal rings:

```rust
use ringmpsc_rs::{AlignedAllocator, Channel, Config};
use std::sync::Arc;
use std::thread;

let config = Config::default();
let channel = Arc::new(
    Channel::<u64, AlignedAllocator<128>>::new_in(config, AlignedAllocator::<128>)
);

// Each producer gets a ring with 128-byte-aligned backing buffer
let ch = Arc::clone(&channel);
let handle = thread::spawn(move || {
    let producer = ch.register().unwrap();
    producer.push(42);
});

channel.consume_all(|val| println!("{}", val));
handle.join().unwrap();
```

The allocator is cloned once per ring at `Channel::new_in()` time. For ZST allocators like `HeapAllocator` and `AlignedAllocator`, cloning is free.

## Implementing Your Own Allocator

Here's a complete example of a custom `VecAllocator`:

```rust
use ringmpsc_rs::BufferAllocator;
use std::mem::MaybeUninit;
use std::ops::{Deref, DerefMut};

/// Buffer wrapper backed by Vec.
struct VecBuffer<T> {
    inner: Vec<MaybeUninit<T>>,
}

impl<T> Deref for VecBuffer<T> {
    type Target = [MaybeUninit<T>];
    fn deref(&self) -> &[MaybeUninit<T>] { &self.inner }
}

impl<T> DerefMut for VecBuffer<T> {
    fn deref_mut(&mut self) -> &mut [MaybeUninit<T>] { &mut self.inner }
}

/// Custom allocator using Vec internally.
#[derive(Clone, Copy, Debug, Default)]
struct VecAllocator;

// Safety: allocate() returns a buffer of exactly `capacity` elements.
// Vec::with_capacity + resize_with guarantees the length.
unsafe impl BufferAllocator for VecAllocator {
    type Buffer<T> = VecBuffer<T>;

    fn allocate<T>(&self, capacity: usize) -> VecBuffer<T> {
        let mut inner = Vec::with_capacity(capacity);
        inner.resize_with(capacity, MaybeUninit::uninit);
        VecBuffer { inner }
    }
}
```

**Safety checklist** — before writing `unsafe impl BufferAllocator`:

| Requirement | How to verify |
|---|---|
| `allocate(n)` returns exactly `n` elements | Assert `buffer.len() == n` in a test |
| Memory is valid for the buffer's lifetime | Ownership model — buffer owns allocation |
| `Deref`/`DerefMut` point to contiguous slice | Use `&[MaybeUninit<T>]` / `&mut [MaybeUninit<T>]` |
| `Drop` deallocates correctly | Rust's ownership (Vec, Box) handles this automatically |

A runnable example is at [crates/ringmpsc/examples/custom_allocator.rs](../crates/ringmpsc/examples/custom_allocator.rs).

## Invariants and Safety

The allocator subsystem is governed by three formal invariants in [spec.md](../crates/ringmpsc/spec.md):

| Invariant | Description | Enforcement |
|---|---|---|
| **INV-MEM-04** | Allocator Safety Contract — the four requirements above | `unsafe trait` keyword |
| **INV-ALLOC-01** | `AlignedAllocator<ALIGN>` produces aligned pointers | `debug_assert!` in `allocate()` + runtime test |
| **INV-ALLOC-02** | `HeapAllocator` is a ZST (zero overhead) | Compile-time `const` assertion |

The corresponding debug assertion macros are defined in [crates/ringmpsc/src/invariants.rs](../crates/ringmpsc/src/invariants.rs):

```rust
// INV-ALLOC-01: runtime alignment check (debug builds only)
macro_rules! debug_assert_aligned {
    ($ptr:expr, $align:expr) => {
        debug_assert!(
            ($ptr as usize) % $align == 0,
            "INV-ALLOC-01 violated: pointer {:p} not aligned to {} bytes",
            $ptr, $align
        )
    };
}

// INV-ALLOC-02: compile-time ZST check
macro_rules! static_assert_zst {
    ($ty:ty) => {
        const _: () = assert!(
            std::mem::size_of::<$ty>() == 0,
            "INV-ALLOC-02 violated: HeapAllocator is not a ZST"
        );
    };
}
```

These macros are active in debug builds (`#[cfg(debug_assertions)]`) and compile to no-ops in release.

## How Alignment Works Internally

`AlignedAllocator<ALIGN>` uses a pointer-bumping technique to guarantee alignment within a standard heap allocation:

```text
Vec<u8> allocation (size = elem_bytes * capacity + ALIGN - 1):
┌────────┬───────────────────────────────────────────┬────────┐
│ padding│  aligned region (capacity × sizeof(T))    │ slack  │
│ 0..127 │  ← ptr starts here (ptr % ALIGN == 0)    │        │
└────────┴───────────────────────────────────────────┴────────┘
```

The implementation in [crates/ringmpsc/src/allocator.rs](../crates/ringmpsc/src/allocator.rs):

```rust
fn allocate<T>(&self, capacity: usize) -> AlignedBuffer<T, ALIGN> {
    assert!(ALIGN.is_power_of_two(), "ALIGN must be a power of two");
    assert!(ALIGN >= std::mem::align_of::<MaybeUninit<T>>());

    let elem_size = std::mem::size_of::<MaybeUninit<T>>();
    let total_bytes = elem_size.checked_mul(capacity).expect("capacity overflow");

    // Over-allocate by ALIGN - 1 bytes for alignment padding
    let alloc_bytes = total_bytes + ALIGN - 1;
    let mut backing = Vec::<u8>::with_capacity(alloc_bytes);
    unsafe { backing.set_len(alloc_bytes); }

    // Round up to nearest aligned address
    let raw = backing.as_mut_ptr() as usize;
    let aligned = (raw + ALIGN - 1) & !(ALIGN - 1);
    let ptr = aligned as *mut MaybeUninit<T>;

    debug_assert_eq!(ptr as usize % ALIGN, 0, "INV-ALLOC-01");

    AlignedBuffer { ptr, len: capacity, _backing: backing }
}
```

The `_backing: Vec<u8>` field keeps the original allocation alive. When `AlignedBuffer` is dropped, the `Vec` is dropped, freeing the memory. The aligned `ptr` is never freed directly — it points into the interior of `_backing`.

## Performance Considerations

| Allocator | Overhead | Best for |
|---|---|---|
| `HeapAllocator` | Zero (ZST, monomorphized away) | Default; identical to pre-allocator code |
| `AlignedAllocator<64>` | ~63 bytes slack per ring | Single-cache-line alignment |
| `AlignedAllocator<128>` | ~127 bytes slack per ring | Intel false-sharing prevention (prefetcher reads 2 lines) |
| `AlignedAllocator<2097152>` | ~2 MiB slack per ring | Huge-page TLB optimization |
| Custom arena | Depends on implementation | Pre-allocated pools, latency-sensitive systems |

**Important notes:**

- Alignment overhead is a **one-time cost at construction** — it does not affect per-message throughput.
- The allocator is consumed at construction; only the buffer is stored in the ring. No per-operation allocator calls.
- `Channel::new_in()` clones the allocator once per ring (`max_producers` times). For ZST allocators this is free.

## Testing

The allocator subsystem has 22 tests in [crates/ringmpsc/tests/allocator_tests.rs](../crates/ringmpsc/tests/allocator_tests.rs):

| Test Category | Count | What it verifies |
|---|---|---|
| `HeapAllocator` | 5 | Basic SPSC, reserve/commit, channel, multi-producer, wrap-around |
| `VecAllocator` | 5 | Same scenarios with a custom allocator |
| `AlignedAllocator` | 8 | Basic ops, **actual pointer alignment**, reserve/commit, channel, wrap-around, drop safety, concurrent stress |
| `BumpAllocator` (bumpalo) | 4 | Arena allocator integration, channel, concurrent |

Run the tests:

```bash
cargo test -p ringmpsc-rs --test allocator_tests --release
```

Benchmarks comparing allocator strategies are in [crates/ringmpsc/benches/allocator.rs](../crates/ringmpsc/benches/allocator.rs):

```bash
cargo bench -p ringmpsc-rs --bench allocator
```

## Formal Verification (Quint Model)

The allocator invariants are formally verified in the Quint specification [crates/ringmpsc/tla/RingSPSC.qnt](../crates/ringmpsc/tla/RingSPSC.qnt). The model tracks allocator-related state alongside the lock-free protocol:

### Modeled Invariants

| Spec Invariant | Quint Element | Description |
|---|---|---|
| **INV-MEM-04** | `allocatorCapacityCorrect` | `buffer_capacity == CAPACITY` — allocator provides exactly the expected number of slots |
| **INV-ALLOC-01** | `alignmentGuarantee` | `buffer_aligned == true` — buffer pointer alignment (structural in Rust, modeled as flag) |
| **INV-ALLOC-02** | `zeroOverheadDefault` | `allocator_zst == true` — HeapAllocator is ZST (structural in Rust, modeled as flag) |
| **INV-INIT-01** | `initializedRange` | `initialized == { (hd+k) % CAPACITY : k ∈ 0..count-1 }` — buffer slot initialization tracked via set. *Note: this invariant bridges the allocator and ring protocol domains — it verifies that the set of initialized buffer slots (an allocator-level concern) matches the range implied by the head/tail sequence numbers (a protocol-level concern).* |

### State Variables

The Quint model adds four state variables for allocator verification:

```
buffer_capacity : int       — allocated buffer size (must equal CAPACITY)
initialized     : Set[int]  — set of buffer indices holding valid data
buffer_aligned  : bool      — alignment guarantee flag
allocator_zst   : bool      — zero-overhead default flag
```

The `initialized` set is the most interesting: `producerWrite` adds `tl % CAPACITY` to the set, and `consumerAdvance` removes `hd % CAPACITY`. The `initializedRange` invariant verifies that this set always matches the expected range derived from `[hd, tl)`.

### Quint Tests

The specification includes allocator-specific embedded tests:

| Quint Test | Invariant | What it verifies |
|---|---|---|
| `allocatorCapacityAtInit` | INV-MEM-04 | Buffer capacity equals CAPACITY at construction |
| `allocatorCapacityStableAcrossOps` | INV-MEM-04 | Capacity unchanged after produce/consume |
| `alignmentAtInit` | INV-ALLOC-01 | Alignment flag set at construction |
| `zeroOverheadAtInit` | INV-ALLOC-02 | ZST flag set at construction |
| `producerWriteInitializesSlot` | INV-INIT-01 | Write marks correct slot index as initialized |
| `consumerAdvanceUninitializesSlot` | INV-INIT-01 | Consume marks correct slot index as uninitialized |
| `initializedRangeWrapAround` | INV-INIT-01 | Modular arithmetic correct after wrap-around |
| `emptyRingNoInitializedSlots` | INV-INIT-01 | Empty ring has empty initialized set |
| `allocatorInvariantsThroughCycle` | All | Full safety invariant at every step of a cycle |

Run the Quint tests:

```bash
cd crates/ringmpsc/tla

# Embedded tests (fast — Rust backend)
quint test RingSPSC.qnt --main=RingSPSC

# Simulation with all invariants (including allocator)
quint run RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant

# Exhaustive model checking (TLC backend)
quint verify RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant --backend=tlc
```

### Model-Based Testing (quint-connect)

The Rust MBT driver in [crates/ringmpsc/tests/quint_mbt.rs](../crates/ringmpsc/tests/quint_mbt.rs) tracks all allocator state and compares it against the Quint specification at every step:

| Quint variable | Rust driver field | Ring correspondence |
|---|---|---|
| `buffer_capacity` | `buffer_capacity` | `CAPACITY` (INV-MEM-04) |
| `initialized` | `initialized_slots: BTreeSet<u64>` | Slot initialization tracking (INV-INIT-01) |
| `buffer_aligned` | `buffer_aligned: bool` | `true` for HeapAllocator (INV-ALLOC-01) |
| `allocator_zst` | `allocator_zst: bool` | `true` for HeapAllocator (INV-ALLOC-02) |

Run the MBT tests:

```bash
cargo test -p ringmpsc-rs --test quint_mbt --features quint-mbt --release
```

## Code Map

| File | Purpose |
|---|---|
| [crates/ringmpsc/src/allocator.rs](../crates/ringmpsc/src/allocator.rs) | `BufferAllocator` trait, `HeapAllocator`, `AlignedAllocator`, `StdAllocator` |
| [crates/ringmpsc/src/ring.rs](../crates/ringmpsc/src/ring.rs) | `Ring<T, A>` — generic over allocator, `new_in()` constructor |
| [crates/ringmpsc/src/channel.rs](../crates/ringmpsc/src/channel.rs) | `Channel<T, A>` — allocator propagated to per-producer rings |
| [crates/ringmpsc/src/invariants.rs](../crates/ringmpsc/src/invariants.rs) | `debug_assert_aligned!`, `static_assert_zst!` macros |
| [crates/ringmpsc/src/lib.rs](../crates/ringmpsc/src/lib.rs) | Public exports: `AlignedAllocator`, `BufferAllocator`, `HeapAllocator`, `StdAllocator` |
| [crates/ringmpsc/spec.md](../crates/ringmpsc/spec.md) | INV-MEM-04, INV-ALLOC-01, INV-ALLOC-02 specifications |
| [crates/ringmpsc/tla/RingSPSC.qnt](../crates/ringmpsc/tla/RingSPSC.qnt) | Quint formal spec — allocator invariants, `initialized` set, model checking |
| [crates/ringmpsc/tests/quint_mbt.rs](../crates/ringmpsc/tests/quint_mbt.rs) | Quint model-based testing driver — allocator state tracking via `quint-connect` |
| [crates/ringmpsc/tests/allocator_tests.rs](../crates/ringmpsc/tests/allocator_tests.rs) | 22 allocator tests |
| [crates/ringmpsc/benches/allocator.rs](../crates/ringmpsc/benches/allocator.rs) | Criterion benchmarks (single-thread, SPSC, MPSC) |
| [crates/ringmpsc/examples/custom_allocator.rs](../crates/ringmpsc/examples/custom_allocator.rs) | Runnable demo comparing all allocator strategies |
