//! Custom allocator support for ring buffer memory.
//!
//! The [`BufferAllocator`] trait abstracts over how the ring buffer's
//! backing memory is allocated. This enables use of custom allocators
//! (huge pages, NUMA-aware, arena, etc.) without modifying the core
//! ring buffer logic.
//!
//! The default [`HeapAllocator`] produces identical behavior and machine
//! code to the pre-allocator version — it is a zero-sized type that adds
//! zero bytes to struct layout.
//!
//! # Nightly: `std::alloc::Allocator` bridge
//!
//! With the `allocator-api` feature (requires nightly Rust), the
//! [`StdAllocator`] adapter wraps any `std::alloc::Allocator` into a
//! `BufferAllocator`.

use std::mem::MaybeUninit;
use std::ops::{Deref, DerefMut};

/// Trait for allocating ring buffer backing memory.
///
/// The ring buffer stores its data in a contiguous buffer of
/// `MaybeUninit<T>` slots. This trait controls how that buffer is
/// allocated and deallocated.
///
/// # Safety
///
/// Implementations must guarantee:
/// 1. `allocate(capacity)` returns a buffer of exactly `capacity` elements.
/// 2. The memory is valid for reads and writes for the buffer's lifetime.
/// 3. The buffer's `Deref`/`DerefMut` targets are contiguous slices.
/// 4. The buffer's `Drop` correctly deallocates the memory.
///
/// Violating these invariants causes undefined behavior in the ring
/// buffer's unsafe hot path (INV-INIT-01).
pub unsafe trait BufferAllocator: Send + Sync {
    /// The owned buffer type returned by the allocator.
    ///
    /// Must dereference to a contiguous `[MaybeUninit<T>]` slice and
    /// handle its own deallocation on drop.
    type Buffer<T>: Deref<Target = [MaybeUninit<T>]> + DerefMut;

    /// Allocate a buffer of `capacity` uninitialized elements.
    ///
    /// # Panics
    ///
    /// May panic on allocation failure (out of memory).
    fn allocate<T>(&self, capacity: usize) -> Self::Buffer<T>;
}

/// Default heap allocator (zero-sized type).
///
/// Produces `Box<[MaybeUninit<T>]>` via `Vec::into_boxed_slice()`,
/// identical to the pre-allocator allocation path. Being a ZST,
/// it adds zero bytes to any struct that contains it.
#[derive(Clone, Copy, Debug, Default)]
pub struct HeapAllocator;

// INV-ALLOC-02: Compile-time proof that HeapAllocator is a ZST.
const _: () = assert!(
    std::mem::size_of::<HeapAllocator>() == 0,
    "INV-ALLOC-02 violated: HeapAllocator must be a zero-sized type"
);

// Safety: allocate() returns a boxed slice of exactly `capacity` elements.
// Vec::with_capacity + resize_with guarantees the length. into_boxed_slice()
// produces a contiguous, valid allocation. Box handles deallocation.
unsafe impl BufferAllocator for HeapAllocator {
    type Buffer<T> = Box<[MaybeUninit<T>]>;

    fn allocate<T>(&self, capacity: usize) -> Box<[MaybeUninit<T>]> {
        let mut buffer = Vec::with_capacity(capacity);
        buffer.resize_with(capacity, MaybeUninit::uninit);
        buffer.into_boxed_slice()
    }
}

// ---- Nightly bridge (feature: allocator-api) ----

#[cfg(feature = "allocator-api")]
mod nightly {
    use super::*;
    use std::alloc::Allocator;

    /// Adapter that bridges a nightly [`std::alloc::Allocator`] to
    /// [`BufferAllocator`].
    ///
    /// # Example (nightly only)
    ///
    /// ```ignore
    /// #![feature(allocator_api)]
    /// use ringmpsc_rs::{Config, Ring, StdAllocator};
    /// use std::alloc::Global;
    ///
    /// let ring = Ring::new_in(Config::default(), StdAllocator(Global));
    /// ```
    #[derive(Debug)]
    pub struct StdAllocator<A: Allocator>(pub A);

    impl<A: Allocator + Clone> Clone for StdAllocator<A> {
        fn clone(&self) -> Self {
            StdAllocator(self.0.clone())
        }
    }

    // Safety: Delegates to std::alloc::Allocator which upholds the same
    // memory validity guarantees. Vec::with_capacity_in + resize_with
    // produces exactly `capacity` elements. Box<[T], A> handles
    // deallocation via the stored allocator.
    unsafe impl<A: Allocator + Clone + Send + Sync> BufferAllocator for StdAllocator<A>
    where
        A: 'static,
    {
        type Buffer<T> = Box<[MaybeUninit<T>], A>;

        fn allocate<T>(&self, capacity: usize) -> Box<[MaybeUninit<T>], A> {
            let mut v = Vec::with_capacity_in(capacity, self.0.clone());
            v.resize_with(capacity, MaybeUninit::uninit);
            v.into_boxed_slice()
        }
    }
}

#[cfg(feature = "allocator-api")]
pub use nightly::StdAllocator;

// ---- Aligned allocator (cache-line / huge-page) ----

/// An allocator that produces allocations aligned to a specified boundary.
///
/// This is useful for:
/// - **Cache-line alignment** (64 or 128 bytes) to prevent false sharing
/// - **Huge pages** (2 MiB alignment) to reduce TLB misses
/// - **NUMA-aware** placement when combined with `mmap`/`madvise`
///
/// The alignment `ALIGN` must be a power of two and ≥ `align_of::<T>()`.
///
/// # Example
///
/// ```
/// use ringmpsc_rs::{AlignedAllocator, Config, Ring};
///
/// // 128-byte aligned (two cache lines — eliminates false sharing)
/// let ring = Ring::<u64, AlignedAllocator<128>>::new_in(
///     Config::default(),
///     AlignedAllocator::<128>,
/// );
/// ring.push(42);
/// let mut val = 0u64;
/// ring.consume_batch(|item| val = *item);
/// assert_eq!(val, 42);
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct AlignedAllocator<const ALIGN: usize>;

/// Buffer wrapper whose backing storage is aligned to `ALIGN` bytes.
///
/// Internally allocates a `Vec<u8>` with extra padding, then hands out
/// a `&mut [MaybeUninit<T>]` slice that starts at the aligned offset.
/// The original allocation is kept alive for `Drop`.
pub struct AlignedBuffer<T, const ALIGN: usize> {
    /// Raw pointer to the aligned region of `MaybeUninit<T>` elements.
    ptr: *mut MaybeUninit<T>,
    /// Number of `MaybeUninit<T>` elements.
    len: usize,
    /// Backing byte allocation (kept alive for `Drop`).
    _backing: Vec<u8>,
}

// Safety: AlignedBuffer owns its allocation and can be sent across threads.
unsafe impl<T: Send, const ALIGN: usize> Send for AlignedBuffer<T, ALIGN> {}

impl<T, const ALIGN: usize> Deref for AlignedBuffer<T, ALIGN> {
    type Target = [MaybeUninit<T>];

    fn deref(&self) -> &[MaybeUninit<T>] {
        // Safety: ptr is valid for `len` elements for the lifetime of `_backing`.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl<T, const ALIGN: usize> DerefMut for AlignedBuffer<T, ALIGN> {
    fn deref_mut(&mut self) -> &mut [MaybeUninit<T>] {
        // Safety: ptr is valid for `len` elements and we have &mut self.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

// Safety:
// - `allocate()` produces a buffer of exactly `capacity` elements.
// - The aligned pointer is computed from a large-enough byte Vec.
// - The Vec stays alive (stored as `_backing`) until the buffer is dropped.
// - ALIGN must be a power of two (asserted at runtime).
unsafe impl<const ALIGN: usize> BufferAllocator for AlignedAllocator<ALIGN> {
    type Buffer<T> = AlignedBuffer<T, ALIGN>;

    fn allocate<T>(&self, capacity: usize) -> AlignedBuffer<T, ALIGN> {
        assert!(ALIGN.is_power_of_two(), "ALIGN must be a power of two");
        assert!(
            ALIGN >= std::mem::align_of::<MaybeUninit<T>>(),
            "ALIGN ({}) must be >= align_of::<T>() ({})",
            ALIGN,
            std::mem::align_of::<MaybeUninit<T>>()
        );

        let elem_size = std::mem::size_of::<MaybeUninit<T>>();
        let total_bytes = elem_size.checked_mul(capacity).expect("capacity overflow");

        // Allocate enough bytes + padding for alignment.
        // We need up to (ALIGN - 1) extra bytes so we can round up the pointer.
        let alloc_bytes = total_bytes + ALIGN - 1;
        let mut backing = Vec::<u8>::with_capacity(alloc_bytes);
        // Safety: we will only access [aligned_ptr .. aligned_ptr + total_bytes],
        // which fits within the allocation.
        #[allow(clippy::uninit_vec)]
        unsafe {
            backing.set_len(alloc_bytes);
        }

        // Compute aligned pointer within the backing allocation.
        let raw = backing.as_mut_ptr() as usize;
        let aligned = (raw + ALIGN - 1) & !(ALIGN - 1);
        let ptr = aligned as *mut MaybeUninit<T>;

        // Verify alignment (this is a debug-mode sanity check).
        debug_assert_eq!(ptr as usize % ALIGN, 0, "INV-ALLOC-01: alignment violated");

        AlignedBuffer {
            ptr,
            len: capacity,
            _backing: backing,
        }
    }
}
