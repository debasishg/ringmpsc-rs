//! Stack-allocated SPSC ring buffer with compile-time capacity.
//!
//! This module provides [`StackRing<T, N>`], a lock-free single-producer single-consumer
//! ring buffer where the buffer is embedded directly in the struct (no heap allocation).
//!
//! # Why Stack Allocation?
//!
//! The heap-based [`Ring<T>`] has excellent performance, but incurs:
//! - **Pointer indirection**: Every buffer access dereferences `buffer_ptr`
//! - **Allocation latency**: Initial `alloc()` call during construction
//! - **Cache unpredictability**: Buffer may land anywhere in memory
//!
//! `StackRing<T, N>` eliminates these by embedding the buffer directly, making
//! `buffer[idx]` a simple base+offset calculation the compiler can constant-fold.
//!
//! # Usage
//!
//! ```ignore
//! use ringmpsc_rs::StackRing;
//!
//! // 4K slots, stack-allocated
//! let ring: StackRing<u64, 4096> = StackRing::new();
//!
//! // Producer (unsafe due to raw pointer API)
//! unsafe {
//!     if let Some((ptr, len)) = ring.reserve(10) {
//!         for i in 0..len {
//!             *ptr.add(i) = i as u64;
//!         }
//!         ring.commit(len);
//!     }
//! }
//!
//! // Consumer
//! unsafe {
//!     ring.consume_batch(|value| {
//!         println!("Got: {}", value);
//!     });
//! }
//! ```
//!
//! # Size Constraints
//!
//! `StackRing` lives on the stack (or in a `Box` if you need heap). Be mindful of sizes:
//! - `StackRing<u64, 4096>` ≈ 33KB (safe on all platforms)
//! - `StackRing<u64, 16384>` ≈ 131KB (safe on Linux/macOS, careful on Windows)
//! - `StackRing<u64, 65536>` ≈ 524KB (may overflow default thread stacks)
//!
//! For larger buffers, use the heap-based [`Ring<T>`] or `Box<StackRing<T, N>>`.

use crate::invariants::{
    debug_assert_bounded_count, debug_assert_head_not_past_tail, debug_assert_initialized_read,
    debug_assert_monotonic, debug_assert_no_wrap,
};

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

// =============================================================================
// COMPILE-TIME ASSERTIONS
// =============================================================================

/// Compile-time assertion that N is a power of 2.
/// This is required for efficient masking (index & (N-1)) instead of modulo.
const fn assert_power_of_two<const N: usize>() {
    assert!(N > 0, "StackRing capacity must be > 0");
    assert!(N.is_power_of_two(), "StackRing capacity must be a power of 2");
}

// =============================================================================
// CACHE LINE ALIGNMENT
// =============================================================================

/// Wrapper type that ensures 128-byte alignment to prevent prefetcher-induced
/// false sharing on Intel/AMD CPUs (which may prefetch adjacent cache lines).
#[repr(C)]
#[repr(align(128))]
struct CacheAligned<T> {
    value: T,
}

impl<T> CacheAligned<T> {
    const fn new(value: T) -> Self {
        Self { value }
    }
}

impl<T> std::ops::Deref for CacheAligned<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T> std::ops::DerefMut for CacheAligned<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}

// =============================================================================
// STACK RING BUFFER
// =============================================================================

/// A stack-allocated SPSC ring buffer with compile-time capacity.
///
/// Unlike [`Ring<T>`] which uses a heap-allocated buffer via `buffer_ptr`,
/// `StackRing` embeds the buffer directly, making `buffer[idx]` a simple
/// base+offset calculation that the compiler can constant-fold.
///
/// # Type Parameters
///
/// - `T`: The element type
/// - `N`: The buffer capacity (must be a power of 2)
///
/// # Memory Layout
///
/// The struct is laid out with hot fields separated by 128 bytes:
///
/// ```text
/// ┌────────────────────────────────────────────────────────────────────┐
/// │ Producer hot (128B aligned)                                        │
/// │   tail: AtomicU64          ← Producer writes, Consumer reads       │
/// │   cached_head: u64         ← Producer-local cache                  │
/// ├────────────────────────────────────────────────────────────────────┤
/// │ Consumer hot (128B aligned)                                        │
/// │   head: AtomicU64          ← Consumer writes, Producer reads       │
/// │   cached_tail: u64         ← Consumer-local cache                  │
/// ├────────────────────────────────────────────────────────────────────┤
/// │ Cold state                                                         │
/// │   closed: AtomicBool                                               │
/// ├────────────────────────────────────────────────────────────────────┤
/// │ Data buffer (inline, no pointer indirection)                       │
/// │   [UnsafeCell<MaybeUninit<T>>; N]                                  │
/// └────────────────────────────────────────────────────────────────────┘
/// ```
#[repr(C)]
pub struct StackRing<T, const N: usize> {
    // === PRODUCER HOT === (128-byte aligned)
    /// Tail sequence number (written by producer, read by consumer)
    tail: CacheAligned<AtomicU64>,
    /// Producer's cached view of head (avoids cross-core reads)
    cached_head: UnsafeCell<u64>,

    // === CONSUMER HOT === (128-byte aligned)  
    /// Head sequence number (written by consumer, read by producer)
    head: CacheAligned<AtomicU64>,
    /// Consumer's cached view of tail (avoids cross-core reads)
    cached_tail: UnsafeCell<u64>,

    // === COLD STATE ===
    /// Whether the ring is closed
    closed: AtomicBool,

    // === DATA BUFFER === (inline, no heap allocation)
    /// The ring buffer storage, embedded directly in the struct.
    /// Uses `UnsafeCell<MaybeUninit<T>>` for interior mutability without
    /// requiring `T: Default`.
    buffer: [UnsafeCell<MaybeUninit<T>>; N],
}

// Safety: StackRing is Send + Sync as long as T is Send.
// The atomic operations ensure proper synchronization between producer and consumer.
// The SPSC design ensures only one thread writes to producer fields, one to consumer fields.
unsafe impl<T: Send, const N: usize> Send for StackRing<T, N> {}
unsafe impl<T: Send, const N: usize> Sync for StackRing<T, N> {}

impl<T, const N: usize> StackRing<T, N> {
    /// The mask for wrapping indices: `N - 1` (works because N is power of 2)
    const MASK: usize = N - 1;

    /// Creates a new stack-allocated ring buffer.
    ///
    /// # Panics
    ///
    /// Panics at compile time if `N` is not a power of 2.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use ringmpsc_rs::StackRing;
    ///
    /// // 4K slots - fits in L1 cache
    /// let ring: StackRing<u64, 4096> = StackRing::new();
    /// ```
    pub const fn new() -> Self {
        // Compile-time assertion
        assert_power_of_two::<N>();

        Self {
            tail: CacheAligned::new(AtomicU64::new(0)),
            cached_head: UnsafeCell::new(0),
            head: CacheAligned::new(AtomicU64::new(0)),
            cached_tail: UnsafeCell::new(0),
            closed: AtomicBool::new(false),
            // SAFETY: MaybeUninit<T> does not require initialization
            // This is the standard pattern for const-initializing arrays of MaybeUninit
            buffer: unsafe { MaybeUninit::uninit().assume_init() },
        }
    }

    // =========================================================================
    // STATUS
    // =========================================================================

    /// Returns the ring buffer capacity.
    #[inline]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Returns the current number of items in the ring.
    #[inline]
    pub fn len(&self) -> usize {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Relaxed);
        tail.wrapping_sub(head) as usize
    }

    /// Returns true if the ring is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.tail.load(Ordering::Relaxed) == self.head.load(Ordering::Relaxed)
    }

    /// Returns true if the ring is full.
    #[inline]
    pub fn is_full(&self) -> bool {
        self.len() >= N
    }

    /// Returns true if the ring is closed.
    #[inline]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Closes the ring, preventing further writes.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    // =========================================================================
    // PRODUCER API
    // =========================================================================

    /// Reserve space for writing up to `n` elements.
    ///
    /// Returns `Some((ptr, len))` where:
    /// - `ptr` is a pointer to the first reserved slot
    /// - `len` is the actual number of contiguous slots reserved (may be < n due to wrap)
    ///
    /// Returns `None` if:
    /// - The ring is full
    /// - `n` is 0 or greater than capacity
    /// - The ring is closed
    ///
    /// # Safety
    ///
    /// The caller must:
    /// - Write to exactly `len` slots starting at `ptr` before calling `commit(len)`
    /// - Not hold the pointer across a `commit()` call
    /// - Ensure only one producer thread calls this
    ///
    /// # Example
    ///
    /// ```ignore
    /// unsafe {
    ///     if let Some((ptr, len)) = ring.reserve(10) {
    ///         for i in 0..len {
    ///             *ptr.add(i) = i as u64;
    ///         }
    ///         ring.commit(len);
    ///     }
    /// }
    /// ```
    #[inline]
    pub unsafe fn reserve(&self, n: usize) -> Option<(*mut T, usize)> {
        if n == 0 || n > N || self.is_closed() {
            return None;
        }

        let tail = self.tail.load(Ordering::Relaxed);

        // Fast path: check cached head
        // SAFETY: cached_head is only written by the producer (this code path).
        let cached_head = *self.cached_head.get();
        let used = tail.wrapping_sub(cached_head) as usize;
        let space = N.saturating_sub(used);

        if space >= n {
            return Some(self.make_reservation(tail, n));
        }

        // Slow path: refresh cache from consumer's head
        let head = self.head.load(Ordering::Acquire);
        *self.cached_head.get() = head;

        let used = tail.wrapping_sub(head) as usize;
        let space = N.saturating_sub(used);

        if space < n {
            return None;
        }

        Some(self.make_reservation(tail, n))
    }

    /// Internal: Create reservation pointer and contiguous length.
    #[inline]
    unsafe fn make_reservation(&self, tail: u64, n: usize) -> (*mut T, usize) {
        let idx = (tail as usize) & Self::MASK;
        // Contiguous slots available before wrap-around
        let contiguous = n.min(N - idx);

        let ptr = (*self.buffer.as_ptr().add(idx)).get() as *mut T;
        (ptr, contiguous)
    }

    /// Commit `n` elements that were written after a successful `reserve()`.
    ///
    /// # Safety
    ///
    /// The caller must have written exactly `n` initialized values to the
    /// slots returned by the preceding `reserve()` call.
    #[inline]
    pub fn commit(&self, n: usize) {
        let tail = self.tail.load(Ordering::Relaxed);
        let new_tail = tail.wrapping_add(n as u64);
        let head = self.head.load(Ordering::Relaxed);

        // INV-SEQ-01: Bounded Count - items in ring never exceed capacity
        debug_assert_bounded_count!(new_tail.wrapping_sub(head) as usize, N);

        // INV-SEQ-02: Monotonic Progress - tail only increases
        debug_assert_monotonic!("tail", tail, new_tail);

        // INV-SEQ-03: No wrap-around (detects bugs, not real overflow)
        debug_assert_no_wrap!("tail", tail, new_tail);

        self.tail.store(new_tail, Ordering::Release);
    }

    // =========================================================================
    // CONSUMER API
    // =========================================================================

    /// Peek at available data for reading.
    ///
    /// Returns `Some((ptr, len))` where:
    /// - `ptr` is a pointer to the first readable slot
    /// - `len` is the number of contiguous readable slots (may be < total available due to wrap)
    ///
    /// Returns `None` if the ring is empty.
    ///
    /// # Safety
    ///
    /// The caller must:
    /// - Only read from the returned pointer
    /// - Call `advance(n)` after processing `n` items
    /// - Ensure only one consumer thread calls this
    #[inline]
    pub unsafe fn peek(&self) -> Option<(*const T, usize)> {
        let head = self.head.load(Ordering::Relaxed);

        // Fast path: check cached tail
        let mut cached_tail = *self.cached_tail.get();
        let mut avail = cached_tail.wrapping_sub(head) as usize;

        if avail == 0 {
            // Slow path: refresh cache from producer's tail
            cached_tail = self.tail.load(Ordering::Acquire);
            *self.cached_tail.get() = cached_tail;
            avail = cached_tail.wrapping_sub(head) as usize;

            if avail == 0 {
                return None;
            }
        }

        let idx = (head as usize) & Self::MASK;
        let contiguous = avail.min(N - idx);

        let ptr = (*self.buffer.as_ptr().add(idx)).get() as *const T;
        Some((ptr, contiguous))
    }

    /// Advance the head pointer after reading `n` items.
    ///
    /// # Safety
    ///
    /// The caller must have read (and optionally dropped) exactly `n` items
    /// from the slots returned by the preceding `peek()` call.
    #[inline]
    pub fn advance(&self, n: usize) {
        let head = self.head.load(Ordering::Relaxed);
        let new_head = head.wrapping_add(n as u64);
        let tail = self.tail.load(Ordering::Relaxed);

        // INV-SEQ-01: Bounded Count - can't consume more than available
        debug_assert_head_not_past_tail!(new_head, tail);

        // INV-SEQ-02: Monotonic Progress - head only increases
        debug_assert_monotonic!("head", head, new_head);

        self.head.store(new_head, Ordering::Release);
    }

    /// Process ALL available items with a single head update.
    ///
    /// This is the key optimization from the LMAX Disruptor pattern: amortizes
    /// atomic operations by processing the entire batch before updating the
    /// head pointer once.
    ///
    /// # Safety
    ///
    /// - Must be called from a single consumer thread only
    /// - Items are moved out of the buffer and dropped after the handler returns
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut sum = 0u64;
    /// unsafe {
    ///     ring.consume_batch(|value| {
    ///         sum += value;
    ///     });
    /// }
    /// ```
    #[inline]
    pub unsafe fn consume_batch<F>(&self, mut handler: F) -> usize
    where
        F: FnMut(&T),
    {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);

        let avail = tail.wrapping_sub(head) as usize;
        if avail == 0 {
            return 0;
        }

        let mut pos = head;

        // Process all available items (no atomics in loop!)
        while pos != tail {
            // INV-INIT-01: Verify we're reading from initialized range
            debug_assert_initialized_read!(pos, head, tail);

            let idx = (pos as usize) & Self::MASK;
            // SAFETY: Item was written by producer and published via Release.
            // The Acquire load on tail synchronizes with that Release.
            let item = (*self.buffer.as_ptr().add(idx)).get().cast::<T>().read();
            handler(&item);
            // `item` is dropped here
            pos = pos.wrapping_add(1);
        }

        // Single atomic update for entire batch
        self.head.store(tail, Ordering::Release);

        avail
    }

    /// Process ALL available items, transferring ownership to the handler.
    ///
    /// Similar to [`consume_batch`], but the handler receives ownership of each item.
    ///
    /// # Safety
    ///
    /// - Must be called from a single consumer thread only
    /// - The handler receives ownership and is responsible for dropping the item
    #[inline]
    pub unsafe fn consume_batch_owned<F>(&self, mut handler: F) -> usize
    where
        F: FnMut(T),
    {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);

        let avail = tail.wrapping_sub(head) as usize;
        if avail == 0 {
            return 0;
        }

        let mut pos = head;

        while pos != tail {
            // INV-INIT-01: Verify we're reading from initialized range
            debug_assert_initialized_read!(pos, head, tail);

            let idx = (pos as usize) & Self::MASK;
            let item = (*self.buffer.as_ptr().add(idx)).get().cast::<T>().read();
            handler(item);
            pos = pos.wrapping_add(1);
        }

        self.head.store(tail, Ordering::Release);

        avail
    }

    /// Process up to `max` items with a single head update.
    ///
    /// Useful for real-world processing to limit batch size and avoid long pauses.
    ///
    /// # Safety
    ///
    /// Must be called from a single consumer thread only.
    #[inline]
    pub unsafe fn consume_up_to<F>(&self, max: usize, mut handler: F) -> usize
    where
        F: FnMut(&T),
    {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);

        let avail = tail.wrapping_sub(head) as usize;
        if avail == 0 {
            return 0;
        }

        let to_consume = avail.min(max);
        let end = head.wrapping_add(to_consume as u64);
        let mut pos = head;

        while pos != end {
            // INV-INIT-01: Verify we're reading from initialized range
            debug_assert_initialized_read!(pos, head, tail);

            let idx = (pos as usize) & Self::MASK;
            let item = (*self.buffer.as_ptr().add(idx)).get().cast::<T>().read();
            handler(&item);
            pos = pos.wrapping_add(1);
        }

        self.head.store(end, Ordering::Release);

        to_consume
    }
}

impl<T, const N: usize> Default for StackRing<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Drop for StackRing<T, N> {
    fn drop(&mut self) {
        // Drop any items that haven't been consumed
        let head = *self.head.get_mut();
        let tail = *self.tail.get_mut();

        let mut pos = head;
        while pos != tail {
            let idx = (pos as usize) & Self::MASK;
            // SAFETY: Items in [head, tail) are initialized and owned by us
            unsafe {
                let slot = self.buffer[idx].get_mut();
                slot.assume_init_drop();
            }
            pos = pos.wrapping_add(1);
        }
    }
}

// =============================================================================
// TYPE ALIASES FOR COMMON CONFIGURATIONS
// =============================================================================

/// 4K slots - fits in L1 cache, ideal for low latency (~33KB for u64)
pub type StackRing4K<T> = StackRing<T, 4096>;

/// 8K slots - fits in L2 cache (~65KB for u64)
pub type StackRing8K<T> = StackRing<T, 8192>;

/// 16K slots - fits in L2 cache (~131KB for u64)
pub type StackRing16K<T> = StackRing<T, 16384>;

/// 64K slots - default capacity, matches heap Ring default (~524KB for u64)
pub type StackRing64K<T> = StackRing<T, 65536>;

// =============================================================================
// TESTS
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capacity_must_be_power_of_two() {
        // These should compile fine
        let _ring: StackRing<u64, 4> = StackRing::new();
        let _ring: StackRing<u64, 16> = StackRing::new();
        let _ring: StackRing<u64, 1024> = StackRing::new();
        let _ring: StackRing<u64, 4096> = StackRing::new();
    }

    #[test]
    fn test_basic_reserve_commit_consume() {
        let ring: StackRing<u64, 16> = StackRing::new();

        // Write 4 items
        unsafe {
            let (ptr, len) = ring.reserve(4).expect("reserve failed");
            assert_eq!(len, 4);
            for i in 0..len {
                *ptr.add(i) = (i * 100) as u64;
            }
            ring.commit(len);
        }

        assert_eq!(ring.len(), 4);
        assert!(!ring.is_empty());
        assert!(!ring.is_full());

        // Consume and verify
        let mut values = Vec::new();
        unsafe {
            ring.consume_batch(|v| values.push(*v));
        }

        assert_eq!(values, vec![0, 100, 200, 300]);
        assert!(ring.is_empty());
    }

    #[test]
    fn test_wrap_around() {
        let ring: StackRing<u64, 8> = StackRing::new();

        // Fill to near capacity
        for i in 0..6 {
            unsafe {
                let (ptr, len) = ring.reserve(1).unwrap();
                *ptr = i;
                ring.commit(len);
            }
        }

        // Consume some
        let mut sum = 0u64;
        unsafe {
            ring.consume_up_to(4, |v| sum += v);
        }
        assert_eq!(sum, 6); // 0 + 1 + 2 + 3

        // Add more (should wrap around)
        for i in 10..14 {
            unsafe {
                let (ptr, len) = ring.reserve(1).unwrap();
                *ptr = i;
                ring.commit(len);
            }
        }

        // Consume all remaining
        let mut values = Vec::new();
        unsafe {
            ring.consume_batch(|v| values.push(*v));
        }
        assert_eq!(values, vec![4, 5, 10, 11, 12, 13]);
    }

    #[test]
    fn test_full_ring() {
        let ring: StackRing<u64, 8> = StackRing::new();

        // Fill completely
        for i in 0..8 {
            unsafe {
                let (ptr, len) = ring.reserve(1).unwrap();
                *ptr = i;
                ring.commit(len);
            }
        }

        assert!(ring.is_full());
        assert_eq!(ring.len(), 8);

        // Reserve should fail when full
        unsafe {
            assert!(ring.reserve(1).is_none());
        }
    }

    #[test]
    fn test_close() {
        let ring: StackRing<u64, 8> = StackRing::new();

        assert!(!ring.is_closed());
        ring.close();
        assert!(ring.is_closed());

        // Reserve should fail when closed
        unsafe {
            assert!(ring.reserve(1).is_none());
        }
    }

    #[test]
    fn test_contiguous_reservation() {
        let ring: StackRing<u64, 8> = StackRing::new();

        // Position at index 6
        for i in 0..6 {
            unsafe {
                let (ptr, len) = ring.reserve(1).unwrap();
                *ptr = i;
                ring.commit(len);
            }
        }
        unsafe {
            ring.consume_batch(|_| {});
        }

        // Now head and tail are at 6. Reserve 4 should return 2 (wrap).
        unsafe {
            let (ptr, len) = ring.reserve(4).unwrap();
            assert_eq!(len, 2); // Only 2 contiguous slots before wrap
            *ptr = 100;
            *ptr.add(1) = 101;
            ring.commit(len);
        }

        // Reserve remaining 2
        unsafe {
            let (ptr, len) = ring.reserve(2).unwrap();
            assert_eq!(len, 2);
            *ptr = 102;
            *ptr.add(1) = 103;
            ring.commit(len);
        }

        // Verify all values
        let mut values = Vec::new();
        unsafe {
            ring.consume_batch(|v| values.push(*v));
        }
        assert_eq!(values, vec![100, 101, 102, 103]);
    }

    #[test]
    fn test_drop_unconsumed_items() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        struct DropTracker(#[allow(dead_code)] u64);

        impl Drop for DropTracker {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, Ordering::SeqCst);
            }
        }

        DROP_COUNT.store(0, Ordering::SeqCst);

        {
            let ring: StackRing<DropTracker, 16> = StackRing::new();

            // Write 5 items
            for i in 0..5 {
                unsafe {
                    let (ptr, _) = ring.reserve(1).unwrap();
                    std::ptr::write(ptr, DropTracker(i));
                    ring.commit(1);
                }
            }

            // Consume only 2
            unsafe {
                ring.consume_up_to(2, |_| {});
            }
            assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 2);

            // Ring drops with 3 unconsumed items
        }

        // All 5 should be dropped now
        assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn test_peek_and_advance() {
        let ring: StackRing<u64, 8> = StackRing::new();

        // Write items
        for i in 0..4 {
            unsafe {
                let (ptr, len) = ring.reserve(1).unwrap();
                *ptr = i * 10;
                ring.commit(len);
            }
        }

        // Peek should show items
        unsafe {
            let (ptr, len) = ring.peek().unwrap();
            assert!(len >= 1);
            assert_eq!(*ptr, 0);
        }

        // Advance by 2
        ring.advance(2);
        assert_eq!(ring.len(), 2);

        // Peek again
        unsafe {
            let (ptr, _) = ring.peek().unwrap();
            assert_eq!(*ptr, 20);
        }
    }

    #[test]
    fn test_consume_batch_owned() {
        let ring: StackRing<String, 8> = StackRing::new();

        // Write strings
        for i in 0..3 {
            unsafe {
                let (ptr, _) = ring.reserve(1).unwrap();
                std::ptr::write(ptr, format!("item_{}", i));
                ring.commit(1);
            }
        }

        // Consume with ownership transfer
        let mut collected = Vec::new();
        unsafe {
            ring.consume_batch_owned(|s| collected.push(s));
        }

        assert_eq!(collected, vec!["item_0", "item_1", "item_2"]);
        assert!(ring.is_empty());
    }
}
