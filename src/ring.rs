use crate::{Backoff, Config, Metrics, Reservation};
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// SPSC ring buffer - the core building block.
///
/// A single-producer single-consumer ring buffer with lock-free operations.
/// Optimized with:
/// - 128-byte alignment to prevent false sharing
/// - Cached sequence numbers to minimize cross-core traffic
/// - Batch operations to amortize atomic overhead
#[repr(C)]
pub struct Ring<T> {
    // === PRODUCER HOT === (128-byte aligned)
    /// Tail index (written by producer, read by consumer)
    tail: CacheAligned<AtomicU64>,
    /// Producer's cached view of head (avoids cross-core reads)
    cached_head: CacheAligned<UnsafeCell<u64>>,

    // === CONSUMER HOT === (128-byte aligned)
    /// Head index (written by consumer, read by producer)
    head: CacheAligned<AtomicU64>,
    /// Consumer's cached view of tail (avoids cross-core reads)
    cached_tail: CacheAligned<UnsafeCell<u64>>,

    // === COLD STATE === (rarely accessed)
    /// Whether this ring is active
    active: CacheAligned<AtomicBool>,
    /// Whether this ring is closed
    closed: AtomicBool,
    /// Optional metrics
    metrics: UnsafeCell<Metrics>,

    // === CONFIG ===
    config: Config,

    // === DATA BUFFER === (64-byte aligned)
    /// The actual ring buffer storage
    buffer: UnsafeCell<Vec<MaybeUninit<T>>>,
}

// Safety: Ring is Send + Sync as long as T is Send
// The atomic operations ensure proper synchronization
unsafe impl<T: Send> Send for Ring<T> {}
unsafe impl<T: Send> Sync for Ring<T> {}

impl<T> Ring<T> {
    /// Creates a new ring buffer with the given configuration.
    pub fn new(config: Config) -> Self {
        let capacity = config.capacity();
        let mut buffer = Vec::with_capacity(capacity);
        // Initialize with uninitialized memory
        buffer.resize_with(capacity, MaybeUninit::uninit);

        Self {
            tail: CacheAligned::new(AtomicU64::new(0)),
            cached_head: CacheAligned::new(UnsafeCell::new(0)),
            head: CacheAligned::new(AtomicU64::new(0)),
            cached_tail: CacheAligned::new(UnsafeCell::new(0)),
            active: CacheAligned::new(AtomicBool::new(false)),
            closed: AtomicBool::new(false),
            metrics: UnsafeCell::new(Metrics::new()),
            config,
            buffer: UnsafeCell::new(buffer),
        }
    }

    // ---------------------------------------------------------------------
    // CONSTANTS & STATUS
    // ---------------------------------------------------------------------

    /// Returns the ring buffer capacity.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.config.capacity()
    }

    /// Returns the index mask for wrapping.
    #[inline]
    fn mask(&self) -> usize {
        self.config.mask()
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
        self.len() >= self.capacity()
    }

    /// Returns true if the ring is closed.
    #[inline]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Marks this ring as active.
    pub(crate) fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Release);
    }

    // ---------------------------------------------------------------------
    // PRODUCER API
    // ---------------------------------------------------------------------

    /// Reserve n slots for zero-copy writing. Returns None if full/closed.
    ///
    /// **Important:** The returned `Reservation` may contain **fewer than n items**
    /// if the reservation wraps around the ring buffer. The ring buffer is circular,
    /// and reservations only provide contiguous memory regions. Always check
    /// `reservation.as_mut_slice().len()` to see how many items were actually reserved.
    ///
    /// To send exactly n items, you may need to call `reserve()` multiple times:
    /// ```ignore
    /// let mut sent = 0;
    /// while sent < n {
    ///     if let Some(mut r) = ring.reserve(n - sent) {
    ///         sent += r.as_mut_slice().len();
    ///         // ... write to slice ...
    ///         r.commit();
    ///     }
    /// }
    /// ```
    ///
    /// Fast path uses cached head to avoid cross-core reads.
    /// Slow path refreshes the cache only when needed.
    #[allow(clippy::cast_possible_truncation)]
    pub fn reserve(&self, n: usize) -> Option<Reservation<'_, T>> {
        if n == 0 || n > self.capacity() {
            return None;
        }

        let tail = self.tail.load(Ordering::Relaxed);

        // Fast path: check cached head
        let cached_head = unsafe { *self.cached_head.get() };
        let space = self.capacity().saturating_sub(tail.wrapping_sub(cached_head) as usize);

        if space >= n {
            return Some(self.make_reservation(tail, n));
        }

        // Slow path: refresh cache
        let head = self.head.load(Ordering::Acquire);
        unsafe { *self.cached_head.get() = head; }

        let space = self.capacity().saturating_sub(tail.wrapping_sub(head) as usize);
        if space < n {
            return None;
        }

        Some(self.make_reservation(tail, n))
    }

    /// Reserve with adaptive backoff. Spins, yields, then gives up.
    pub fn reserve_with_backoff(&self, n: usize) -> Option<Reservation<'_, T>> {
        let mut backoff = Backoff::new();
        while !backoff.is_completed() {
            if let Some(r) = self.reserve(n) {
                return Some(r);
            }
            if self.is_closed() {
                return None;
            }
            backoff.snooze();
        }
        None
    }

    /// Internal: Create a reservation for writing.
    fn make_reservation(&self, tail: u64, n: usize) -> Reservation<'_, T> {
        let mask = self.mask();
        let idx = (tail as usize) & mask;
        let contiguous = n.min(self.capacity() - idx);

        // Get mutable slice for writing
        let slice = unsafe {
            let buffer = &mut *self.buffer.get();
            std::slice::from_raw_parts_mut(
                buffer[idx..].as_mut_ptr().cast::<T>(),
                contiguous,
            )
        };

        // Create reservation with commit callback
        let ring_ptr = self as *const Self;
        Reservation::new(slice, ring_ptr)
    }

    /// Internal: Commit n slots after writing. Called by Reservation.
    pub(crate) fn commit_internal(&self, n: usize) {
        let tail = self.tail.load(Ordering::Relaxed);
        self.tail.store(tail.wrapping_add(n as u64), Ordering::Release);

        if self.config.enable_metrics {
            unsafe {
                let metrics = &mut *self.metrics.get();
                metrics.messages_sent += n as u64;
                metrics.batches_sent += 1;
            }
        }
    }

    // ---------------------------------------------------------------------
    // CONSUMER API
    // ---------------------------------------------------------------------

    /// Get readable slice. Returns None if empty.
    #[allow(clippy::cast_possible_truncation)]
    pub fn readable(&self) -> Option<&[T]> {
        let head = self.head.load(Ordering::Relaxed);

        // Fast path: check cached tail
        let mut cached_tail = unsafe { *self.cached_tail.get() };
        let mut avail = cached_tail.wrapping_sub(head) as usize;

        if avail == 0 {
            // Slow path: refresh cache
            cached_tail = self.tail.load(Ordering::Acquire);
            unsafe { *self.cached_tail.get() = cached_tail; }
            avail = cached_tail.wrapping_sub(head) as usize;
            if avail == 0 {
                return None;
            }
        }

        let mask = self.mask();
        let idx = (head as usize) & mask;
        let contiguous = avail.min(self.capacity() - idx);

        unsafe {
            let buffer = &*self.buffer.get();
            Some(std::slice::from_raw_parts(
                buffer[idx..].as_ptr().cast::<T>(),
                contiguous,
            ))
        }
    }

    /// Advance head after reading n items.
    #[inline]
    pub fn advance(&self, n: usize) {
        let head = self.head.load(Ordering::Relaxed);
        self.head.store(head.wrapping_add(n as u64), Ordering::Release);

        if self.config.enable_metrics {
            unsafe {
                let metrics = &mut *self.metrics.get();
                metrics.messages_received += n as u64;
                metrics.batches_received += 1;
            }
        }
    }

    // ---------------------------------------------------------------------
    // BATCH CONSUMPTION (Disruptor Pattern)
    // ---------------------------------------------------------------------

    /// Process ALL available items with a single head update.
    ///
    /// This is the key optimization: amortizes atomic operations by processing
    /// the entire batch before updating the head pointer once.
    #[allow(clippy::cast_possible_truncation)]
    pub fn consume_batch<F>(&self, mut handler: F) -> usize
    where
        F: FnMut(&T),
    {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);

        let avail = tail.wrapping_sub(head) as usize;
        if avail == 0 {
            return 0;
        }

        let mask = self.mask();
        let mut pos = head;
        let mut count = 0;

        // Process all available items (no atomics in loop!)
        while pos != tail {
            let idx = (pos as usize) & mask;
            unsafe {
                let buffer = &*self.buffer.get();
                let item = &*(buffer[idx].as_ptr());
                handler(item);
            }
            pos = pos.wrapping_add(1);
            count += 1;
        }

        // Single atomic update for entire batch
        self.head.store(tail, Ordering::Release);

        if self.config.enable_metrics {
            unsafe {
                let metrics = &mut *self.metrics.get();
                metrics.messages_received += count as u64;
                metrics.batches_received += 1;
            }
        }

        count
    }

    /// Consume up to `max_items` with a single head update.
    ///
    /// Useful for real-world processing where large batches may block too long.
    #[allow(clippy::cast_possible_truncation)]
    pub fn consume_up_to<F>(&self, max_items: usize, mut handler: F) -> usize
    where
        F: FnMut(&T),
    {
        if max_items == 0 {
            return 0;
        }

        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);

        let avail = tail.wrapping_sub(head) as usize;
        if avail == 0 {
            return 0;
        }

        let to_consume = avail.min(max_items);
        let mask = self.mask();
        let mut pos = head;
        let mut count = 0;

        // Process up to max_items
        while count < to_consume {
            let idx = (pos as usize) & mask;
            unsafe {
                let buffer = &*self.buffer.get();
                let item = &*(buffer[idx].as_ptr());
                handler(item);
            }
            pos = pos.wrapping_add(1);
            count += 1;
        }

        // Single atomic update for the batch
        self.head.store(head.wrapping_add(count as u64), Ordering::Release);

        if self.config.enable_metrics {
            unsafe {
                let metrics = &mut *self.metrics.get();
                metrics.messages_received += count as u64;
                metrics.batches_received += 1;
            }
        }

        count
    }

    // ---------------------------------------------------------------------
    // CONVENIENCE WRAPPERS
    // ---------------------------------------------------------------------

    /// Batch send (convenience).
    pub fn send(&self, items: &[T]) -> usize
    where
        T: Copy,
    {
        self.reserve(items.len()).map_or(0, |mut reservation| {
            let slice = reservation.as_mut_slice();
            let n = slice.len();
            slice.copy_from_slice(&items[..n]);
            reservation.commit();
            n
        })
    }

    /// Batch receive (convenience).
    pub fn recv(&self, out: &mut [T]) -> usize
    where
        T: Copy,
    {
        self.readable().map_or(0, |slice| {
            let n = slice.len().min(out.len());
            out[..n].copy_from_slice(&slice[..n]);
            self.advance(n);
            n
        })
    }

    // ---------------------------------------------------------------------
    // LIFECYCLE
    // ---------------------------------------------------------------------

    /// Close the ring, preventing further operations.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    /// Get metrics if enabled.
    pub fn metrics(&self) -> Metrics {
        if self.config.enable_metrics {
            unsafe { *self.metrics.get() }
        } else {
            Metrics::default()
        }
    }
}

impl<T> Drop for Ring<T> {
    fn drop(&mut self) {
        // Drop all initialized items in the ring
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Relaxed);
        let count = tail.wrapping_sub(head) as usize;

        if count > 0 {
            let mask = self.mask();
            let buffer = self.buffer.get_mut();

            for i in 0..count {
                let idx = ((head as usize).wrapping_add(i)) & mask;
                unsafe {
                    ptr::drop_in_place(buffer[idx].as_mut_ptr());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------
// HELPER: 128-byte cache-aligned wrapper
// ---------------------------------------------------------------------

/// Wrapper type that ensures 128-byte alignment to prevent prefetcher-induced
/// false sharing on Intel/AMD CPUs (which may prefetch adjacent cache lines).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ring_basic_reserve_commit() {
        let ring = Ring::<u64>::new(Config::default());

        // Write
        if let Some(mut r) = ring.reserve(4) {
            let slice = r.as_mut_slice();
            slice[0] = 100;
            slice[1] = 200;
            slice[2] = 300;
            slice[3] = 400;
            r.commit();
        }

        assert_eq!(ring.len(), 4);

        // Read
        if let Some(slice) = ring.readable() {
            assert_eq!(slice[0], 100);
            assert_eq!(slice[3], 400);
            ring.advance(4);
        }

        assert!(ring.is_empty());
    }

    #[test]
    fn test_ring_batch_consumption() {
        let ring = Ring::<u64>::new(Config::default());

        // Write 10 items
        for i in 0..10 {
            if let Some(mut r) = ring.reserve(1) {
                r.as_mut_slice()[0] = i * 10;
                r.commit();
            }
        }

        // Consume all at once
        let mut sum = 0u64;
        let consumed = ring.consume_batch(|item| sum += item);

        assert_eq!(consumed, 10);
        assert_eq!(sum, 0 + 10 + 20 + 30 + 40 + 50 + 60 + 70 + 80 + 90);
        assert!(ring.is_empty());
    }

    #[test]
    fn test_ring_consume_up_to() {
        let ring = Ring::<u64>::new(Config::default());

        // Write 10 items
        for i in 0..10 {
            if let Some(mut r) = ring.reserve(1) {
                r.as_mut_slice()[0] = i * 10;
                r.commit();
            }
        }

        // Consume up to 5
        let mut sum = 0u64;
        let consumed = ring.consume_up_to(5, |item| sum += item);

        assert_eq!(consumed, 5);
        assert_eq!(sum, 0 + 10 + 20 + 30 + 40);
        assert_eq!(ring.len(), 5); // 5 left

        // Consume remaining
        sum = 0;
        let consumed2 = ring.consume_up_to(10, |item| sum += item);
        assert_eq!(consumed2, 5);
        assert_eq!(sum, 50 + 60 + 70 + 80 + 90);
        assert!(ring.is_empty());
    }

    #[test]
    fn test_ring_full() {
        let config = Config::new(4, 16, false); // 16 slots
        let ring = Ring::<u64>::new(config);

        // Fill it
        for i in 0..16 {
            if let Some(mut r) = ring.reserve(1) {
                r.as_mut_slice()[0] = i;
                r.commit();
            }
        }

        // Should fail
        assert!(ring.reserve(1).is_none());
    }
}
