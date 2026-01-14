# UB Fix in `make_reservation`

This document explains the undefined behavior (UB) bug found in `make_reservation` in `src/ring.rs`, why it manifested differently in debug vs release builds, and how it was fixed.

## The Bug

### Original Code (Buggy)

```rust
fn make_reservation(&self, tail: u64, n: usize) -> Reservation<'_, T> {
    let mask = self.mask();
    let idx = (tail as usize) & mask;
    let contiguous = n.min(self.capacity() - idx);

    // Prefetch next batch location (hide memory latency)
    let next_idx = ((tail + n as u64) as usize) & mask;
    unsafe {
        let buffer = &*self.buffer.get();
        let _ = &buffer[next_idx];
    }

    // Get mutable slice for writing
    let slice = unsafe {
        let buffer = &mut *self.buffer.get();
        std::slice::from_raw_parts_mut(
            buffer[idx..].as_mut_ptr().cast::<T>(),  // ❌ UB HERE
            contiguous,
        )
    };

    let ring_ptr = self as *const Self;
    Reservation::new(slice, move |count| {
        let ring = unsafe { &*ring_ptr };
        ring.commit_internal(count);
    })
}
```

### The Problem

The critical issue is on this line:

```rust
buffer[idx..].as_mut_ptr().cast::<T>()
```

This code:
1. Takes a pointer to `MaybeUninit<T>` (uninitialized memory)
2. Casts it to a pointer to `T` (assumes initialized memory)
3. Creates a slice `&mut [T]` from uninitialized memory

**This is undefined behavior** because Rust's type system contract says that a `&T` or `&mut T` reference must point to a valid, initialized value of type `T`. By casting `MaybeUninit<T>` to `T`, we're lying to the compiler about the state of the memory.

## Why Debug Crashed but Release Worked

This is a classic example of why UB is dangerous — **it doesn't always crash**.

### Debug Mode: Memory Patterns Expose the Bug

In debug builds, Rust/LLVM:

1. **Initializes memory with poison patterns** (e.g., `0xAA`, `0xDEADBEEF`) to help detect use of uninitialized memory
2. **Doesn't optimize away reads** — every memory access actually happens
3. **Inserts additional checks** and doesn't elide unused operations

When the code treats `MaybeUninit<T>` as `T` and tries to read/use that memory:
- If `T` contains pointers, they point to garbage addresses
- If `T` has a `Drop` implementation, it might try to drop garbage
- If `T` has a vtable (trait objects), dereferencing it causes **SIGSEGV**

### Release Mode: Optimizations Hide the Bug

In release builds with optimizations (`-O2`/`-O3`), LLVM:

1. **Assumes UB never happens** — this is the "UB contract"
2. **Elides dead reads** — if you write to a slot and never read the "old" value, the read is removed
3. **Removes redundant operations** — the optimizer sees you're just writing, so it skips any "read before write" codegen
4. **Doesn't use poison patterns** — memory is whatever was there before (often zeroes from the OS)

The optimizer essentially **optimized away the dangerous operation** because:
1. You allocate `MaybeUninit<T>` slots
2. You immediately write new values via the reservation
3. The optimizer sees no read of the old value → removes any code that would trigger the crash

### Analogy

It's like driving through a red light at 3 AM:
- **Debug mode** = There's a cop watching every intersection (crash)
- **Release mode** = Empty streets, no cops, you "got away with it" (no crash, but still illegal)

The behavior is still undefined — it just happened to not manifest in release. A different optimization level, compiler version, or target architecture could easily cause release builds to crash too.

## The Fix

### Fixed Code

```rust
fn make_reservation(&self, tail: u64, n: usize) -> Reservation<'_, T> {
    let mask = self.mask();
    let idx = (tail as usize) & mask;
    let contiguous = n.min(self.capacity() - idx);

    // Get mutable slice for writing (keeping as MaybeUninit)
    let slice = unsafe {
        let buffer = &mut *self.buffer.get();
        &mut buffer[idx..idx + contiguous]  // ✅ Returns &mut [MaybeUninit<T>]
    };

    // Create reservation with commit callback
    let ring_ptr = self as *const Self;
    Reservation::new(slice, ring_ptr)
}
```

### What Changed

1. **Kept the type as `&mut [MaybeUninit<T>]`** instead of casting to `&mut [T]`
2. **Removed the software prefetch** — hardware prefetchers handle sequential access better on modern CPUs (see project conventions)
3. **Simplified the Reservation API** — now stores `ring_ptr` directly instead of a closure

### Why This is Sound

The type system now correctly represents that:
- The memory is **uninitialized** until the producer writes to it
- Callers must use `MaybeUninit::new(value)` or `.write(value)` to initialize
- The compiler won't generate code that assumes the memory contains valid `T` values

## The Full Initialization Flow

### 1. Producer Side - Initialization

The producer gets a `&mut [MaybeUninit<T>]` and initializes it:

```rust
// Pattern A: MaybeUninit::new() - creates initialized value
reservation.as_mut_slice()[0] = MaybeUninit::new(42);

// Pattern B: MaybeUninit::write() - writes in place, returns &mut T
reservation.as_mut_slice()[0].write(42);

// Pattern C: Convenience method (handles MaybeUninit internally)
producer.push(42);
```

### 2. Commit - Makes Data Visible

After writing, `commit()` advances the tail pointer with `Release` ordering:

```rust
// ring.rs: commit_internal()
self.tail.store(tail.wrapping_add(n as u64), Ordering::Release);
```

### 3. Consumer Side - Reading Initialized Data

The consumer reads with `Acquire` ordering and uses `assume_init_ref()`:

```rust
// ring.rs: consume_batch()
let tail = self.tail.load(Ordering::Acquire);  // Syncs with producer's Release
// ...
let item = buffer[idx].assume_init_ref();  // Safe: producer initialized this
handler(item);
```

### Safety Contract

The **invariant** maintained by the ring buffer:
- Slots between `head` and `tail` are **initialized** (producer wrote + committed)
- Slots outside that range are **uninitialized** (not yet written or already consumed)

The `Release`/`Acquire` ordering ensures the consumer sees the producer's writes **before** reading via `assume_init_ref()`. This is what makes the `unsafe` call sound — by the time the consumer can see a slot (tail has advanced), the data is guaranteed to be initialized.

## API Design: MaybeUninit vs Initialized

Returning `&mut [MaybeUninit<T>]` is the idiomatic choice for a high-performance lock-free channel because:

1. **True zero-copy** — no intermediate copies or moves
2. **Matches the Disruptor pattern** — LMAX Disruptor does exactly this
3. **Target audience expects it** — developers using lock-free primitives understand `MaybeUninit`

For convenience, we also provide high-level APIs that handle `MaybeUninit` internally:

```rust
// Simple API for single items
producer.push(42);  // Handles MaybeUninit internally

// Batch API for Copy types
producer.send(&[1, 2, 3, 4, 5]);  // Handles MaybeUninit internally
```

## Lessons Learned

1. **UB is silent in release, loud in debug** — always test in both modes
2. **Don't lie to the type system** — if memory is uninitialized, keep it as `MaybeUninit<T>`
3. **The optimizer assumes correctness** — UB allows the optimizer to remove "impossible" code paths
4. **`cargo miri`** — use Miri to detect UB that might not manifest as crashes

## References

- [Rust Nomicon: Uninitialized Memory](https://doc.rust-lang.org/nomicon/uninitialized.html)
- [std::mem::MaybeUninit](https://doc.rust-lang.org/std/mem/union.MaybeUninit.html)
- [LMAX Disruptor](https://lmax-exchange.github.io/disruptor/) — the original ring buffer pattern
