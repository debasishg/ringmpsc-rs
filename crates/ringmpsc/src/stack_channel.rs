//! Stack-allocated Multi-Producer Single-Consumer channel using ring decomposition.
//!
//! This module provides [`StackChannel<T, N, P>`], a fully stack-allocated MPSC channel
//! where each producer gets a dedicated [`StackRing`] to eliminate all producer-producer
//! contention.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────┐
//! │                     StackChannel<T, N, P>                           │
//! ├─────────────────────────────────────────────────────────────────────┤
//! │                                                                     │
//! │  Producer 0 ──► [StackRing 0] ──┐                                   │
//! │  Producer 1 ──► [StackRing 1] ──┼──► ONE Consumer (consume_all)     │
//! │  Producer 2 ──► [StackRing 2] ──┤    polls all rings in sequence    │
//! │      ...                        │                                   │
//! │  Producer P ──► [StackRing P] ──┘                                   │
//! │                                                                     │
//! └─────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! **Key points:**
//! - Each producer has a **dedicated SPSC ring** — zero producer-producer contention
//! - **Single consumer** calls `consume_all()` which iterates through all active rings
//! - This is **MPSC = [SPSC] × P**, NOT P independent pairs
//!
//! # Usage
//!
//! ```ignore
//! use ringmpsc_rs::{StackChannel, StackChannelError};
//! use std::thread;
//!
//! // 4K slots per ring, max 4 producers
//! let channel: StackChannel<u64, 4096, 4> = StackChannel::new();
//!
//! // Leak to 'static for thread sharing (or use scoped threads)
//! let channel: &'static _ = Box::leak(Box::new(channel));
//!
//! // Spawn producers
//! let handles: Vec<_> = (0..4).map(|i| {
//!     let producer = channel.register().unwrap();
//!     thread::spawn(move || {
//!         producer.push((i * 1000) as u64);
//!     })
//! }).collect();
//!
//! for h in handles { h.join().unwrap(); }
//!
//! // Consume on main thread
//! let total = channel.consume_all(|v| println!("{}", v));
//! ```
//!
//! # Size Constraints
//!
//! `StackChannel<T, 4096, 16>` with `T = u64` uses ~530KB. For larger configurations,
//! consider the heap-based [`Channel<T>`](crate::Channel) or `Box<StackChannel<T, N, P>>`.

use crate::stack_ring::StackRing;
#[cfg(debug_assertions)]
use crate::invariants::debug_assert_fifo_count;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(debug_assertions)]
use std::sync::atomic::AtomicU64;
use thiserror::Error;

// =============================================================================
// COMPILE-TIME ASSERTIONS
// =============================================================================

/// Compile-time assertion that N is a power of 2.
const fn assert_power_of_two<const N: usize>() {
    assert!(N > 0, "StackChannel ring capacity must be > 0");
    assert!(
        N.is_power_of_two(),
        "StackChannel ring capacity must be a power of 2"
    );
}

/// Compile-time assertion that P is reasonable.
const fn assert_valid_producer_count<const P: usize>() {
    assert!(P > 0, "StackChannel must allow at least 1 producer");
    assert!(P <= 128, "StackChannel max producers should not exceed 128");
}

// =============================================================================
// ERROR TYPES
// =============================================================================

/// Error types for stack channel operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum StackChannelError {
    /// Too many producers registered (exceeds max P).
    #[error("too many producers registered (max: {max})")]
    TooManyProducers {
        /// The configured maximum number of producers.
        max: usize,
    },
    /// Channel is closed.
    #[error("channel is closed")]
    Closed,
}

// =============================================================================
// STACK CHANNEL
// =============================================================================

/// Stack-allocated Multi-Producer Single-Consumer channel.
///
/// Uses ring decomposition: each producer gets a dedicated [`StackRing`],
/// eliminating all producer-producer contention.
///
/// # Type Parameters
///
/// - `T`: Element type
/// - `N`: Ring capacity per producer (must be power of 2)
/// - `P`: Maximum number of producers
///
/// # Memory Layout
///
/// ```text
/// ┌────────────────────────────────────────────────────────┐
/// │ Atomic state                                           │
/// │   producer_count: AtomicUsize                          │
/// │   closed: AtomicBool                                   │
/// ├────────────────────────────────────────────────────────┤
/// │ Ring array (inline, P × StackRing<T, N>)               │
/// │   rings[0]: StackRing<T, N>                            │
/// │   rings[1]: StackRing<T, N>                            │
/// │   ...                                                  │
/// │   rings[P-1]: StackRing<T, N>                          │
/// └────────────────────────────────────────────────────────┘
/// ```
#[repr(C)]
pub struct StackChannel<T, const N: usize, const P: usize> {
    /// Number of registered producers
    producer_count: AtomicUsize,
    /// Whether the channel is closed
    closed: AtomicBool,
    /// Array of SPSC rings, one per producer slot
    rings: [StackRing<T, N>; P],
    /// Per-producer consumption count for FIFO verification (debug only)
    #[cfg(debug_assertions)]
    consumed_counts: [AtomicU64; P],
}

// Safety: StackChannel is Send + Sync as long as T is Send.
// Each producer has exclusive access to its ring's producer API.
// The single consumer accesses all rings but from one thread.
unsafe impl<T: Send, const N: usize, const P: usize> Send for StackChannel<T, N, P> {}
unsafe impl<T: Send, const N: usize, const P: usize> Sync for StackChannel<T, N, P> {}

impl<T, const N: usize, const P: usize> StackChannel<T, N, P> {
    /// Creates a new stack-allocated channel.
    ///
    /// # Panics
    ///
    /// Panics at compile time if:
    /// - `N` is not a power of 2
    /// - `P` is 0 or greater than 128
    ///
    /// # Example
    ///
    /// ```ignore
    /// use ringmpsc_rs::StackChannel;
    ///
    /// // 4K slots per ring, max 4 producers
    /// let channel: StackChannel<u64, 4096, 4> = StackChannel::new();
    /// ```
    pub const fn new() -> Self {
        assert_power_of_two::<N>();
        assert_valid_producer_count::<P>();

        Self {
            producer_count: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            // Initialize P rings using const evaluation
            rings: [const { StackRing::new() }; P],
            // Initialize P consumption counters (debug only)
            #[cfg(debug_assertions)]
            consumed_counts: [const { AtomicU64::new(0) }; P],
        }
    }

    // =========================================================================
    // STATUS
    // =========================================================================

    /// Returns the ring capacity per producer.
    #[inline]
    pub const fn ring_capacity(&self) -> usize {
        N
    }

    /// Returns the maximum number of producers.
    #[inline]
    pub const fn max_producers(&self) -> usize {
        P
    }

    /// Returns the number of registered producers.
    #[inline]
    pub fn producer_count(&self) -> usize {
        self.producer_count.load(Ordering::Acquire)
    }

    /// Returns true if the channel is closed.
    #[inline]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    // =========================================================================
    // PRODUCER REGISTRATION
    // =========================================================================

    /// Register a new producer.
    ///
    /// Returns a [`StackProducer`] handle that can write to a dedicated ring.
    /// Fails if too many producers are registered or the channel is closed.
    ///
    /// # Errors
    ///
    /// - [`StackChannelError::TooManyProducers`] if `P` producers already registered
    /// - [`StackChannelError::Closed`] if the channel is closed
    ///
    /// # Example
    ///
    /// ```ignore
    /// let channel: StackChannel<u64, 4096, 4> = StackChannel::new();
    /// let producer = channel.register().expect("registration failed");
    /// producer.push(42);
    /// ```
    pub fn register(&self) -> Result<StackProducer<'_, T, N, P>, StackChannelError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(StackChannelError::Closed);
        }

        // Atomically claim a slot
        let id = self.producer_count.fetch_add(1, Ordering::SeqCst);

        if id >= P {
            // Roll back - too many producers
            self.producer_count.fetch_sub(1, Ordering::SeqCst);
            return Err(StackChannelError::TooManyProducers { max: P });
        }

        Ok(StackProducer { channel: self, id })
    }

    // =========================================================================
    // CONSUMER API
    // =========================================================================

    /// Consume from all producers - THE FAST PATH.
    ///
    /// Iterates through all registered producers' rings and processes all
    /// available items. Each ring is batch-consumed with a single atomic
    /// head update.
    ///
    /// # Safety Note
    ///
    /// This method is safe because it wraps the unsafe ring operations
    /// internally. The SPSC invariant is maintained: only the registered
    /// producer writes to each ring, only this consumer reads.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut sum = 0u64;
    /// let consumed = channel.consume_all(|value| sum += value);
    /// println!("Consumed {} items, sum = {}", consumed, sum);
    /// ```
    pub fn consume_all<F>(&self, mut handler: F) -> usize
    where
        F: FnMut(&T),
    {
        let count = self.producer_count.load(Ordering::Acquire);
        let mut total = 0;

        for (producer_id, ring) in self.rings[..count].iter().enumerate() {
            // SAFETY: We are the single consumer. Producer writes are
            // synchronized via Release on tail, Acquire here on consume.
            let consumed = unsafe { ring.consume_batch(&mut handler) };

            // INV-CH-03: Verify per-producer FIFO by tracking cumulative count
            #[cfg(debug_assertions)]
            {
                let old_count = self.consumed_counts[producer_id].load(Ordering::Relaxed);
                let new_count = old_count + consumed as u64;
                debug_assert_fifo_count!(producer_id, old_count, new_count);
                self.consumed_counts[producer_id].store(new_count, Ordering::Relaxed);
            }

            total += consumed;
        }

        total
    }

    /// Consume from all producers, transferring ownership.
    ///
    /// Similar to [`consume_all`], but the handler receives ownership of each item.
    /// More efficient for types expensive to clone.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut items = Vec::new();
    /// channel.consume_all_owned(|item| items.push(item));
    /// ```
    pub fn consume_all_owned<F>(&self, mut handler: F) -> usize
    where
        F: FnMut(T),
    {
        let count = self.producer_count.load(Ordering::Acquire);
        let mut total = 0;

        for (producer_id, ring) in self.rings[..count].iter().enumerate() {
            let consumed = unsafe { ring.consume_batch_owned(&mut handler) };

            // INV-CH-03: Verify per-producer FIFO by tracking cumulative count
            #[cfg(debug_assertions)]
            {
                let old_count = self.consumed_counts[producer_id].load(Ordering::Relaxed);
                let new_count = old_count + consumed as u64;
                debug_assert_fifo_count!(producer_id, old_count, new_count);
                self.consumed_counts[producer_id].store(new_count, Ordering::Relaxed);
            }

            total += consumed;
        }

        total
    }

    /// Consume up to `max_total` items from all producers.
    ///
    /// Useful for real-world processing to limit batch size and avoid
    /// long pauses. Prefers earlier producers (ring 0, then 1, etc.).
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Process at most 1000 items per iteration
    /// while !channel.is_closed() {
    ///     channel.consume_all_up_to(1000, |item| process(item));
    ///     std::thread::yield_now();
    /// }
    /// ```
    pub fn consume_all_up_to<F>(&self, max_total: usize, mut handler: F) -> usize
    where
        F: FnMut(&T),
    {
        let count = self.producer_count.load(Ordering::Acquire);
        let mut total = 0;

        for (producer_id, ring) in self.rings[..count].iter().enumerate() {
            if total >= max_total {
                break;
            }
            let remaining = max_total - total;
            let consumed = unsafe { ring.consume_up_to(remaining, &mut handler) };

            // INV-CH-03: Verify per-producer FIFO by tracking cumulative count
            #[cfg(debug_assertions)]
            {
                let old_count = self.consumed_counts[producer_id].load(Ordering::Relaxed);
                let new_count = old_count + consumed as u64;
                debug_assert_fifo_count!(producer_id, old_count, new_count);
                self.consumed_counts[producer_id].store(new_count, Ordering::Relaxed);
            }

            total += consumed;
        }

        total
    }

    /// Get a reference to a specific producer's ring.
    ///
    /// Useful for dedicated consumer patterns where each consumer reads
    /// from a specific ring.
    ///
    /// Returns `None` if `ring_id >= P`.
    #[inline]
    pub fn get_ring(&self, ring_id: usize) -> Option<&StackRing<T, N>> {
        if ring_id < P {
            Some(&self.rings[ring_id])
        } else {
            None
        }
    }

    // =========================================================================
    // LIFECYCLE
    // =========================================================================

    /// Close the channel and all rings.
    ///
    /// After closing:
    /// - `register()` returns `Err(Closed)`
    /// - Existing producers' `push()` returns `false`
    /// - Consumers can still drain remaining items
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let count = self.producer_count.load(Ordering::Acquire);
        for ring in &self.rings[..count] {
            ring.close();
        }
    }
}

impl<T, const N: usize, const P: usize> Default for StackChannel<T, N, P> {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// STACK PRODUCER
// =============================================================================

/// Producer handle for a [`StackChannel`].
///
/// Each producer has exclusive write access to its dedicated ring.
/// The lifetime parameter ties the producer to its channel.
///
/// # Note
///
/// `StackProducer` does NOT implement `Clone`. Cloning would allow multiple
/// threads to write to the same ring, breaking the SPSC invariant.
pub struct StackProducer<'a, T, const N: usize, const P: usize> {
    channel: &'a StackChannel<T, N, P>,
    id: usize,
}

// Safety: StackProducer is Send if T is Send.
// Only this producer writes to its ring; the consumer reads.
unsafe impl<T: Send, const N: usize, const P: usize> Send for StackProducer<'_, T, N, P> {}

impl<'a, T, const N: usize, const P: usize> StackProducer<'a, T, N, P> {
    /// Returns this producer's ID (ring index).
    #[inline]
    pub fn id(&self) -> usize {
        self.id
    }

    /// Returns the ring capacity.
    #[inline]
    pub fn capacity(&self) -> usize {
        N
    }

    /// Reserve space for writing up to `n` elements.
    ///
    /// Returns `Some((ptr, len))` where `len` may be less than `n` due to
    /// wrap-around. Returns `None` if full or closed.
    ///
    /// # Safety
    ///
    /// The caller must write exactly `len` items before calling `commit(len)`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// unsafe {
    ///     if let Some((ptr, len)) = producer.reserve(10) {
    ///         for i in 0..len {
    ///             *ptr.add(i) = i as u64;
    ///         }
    ///         producer.commit(len);
    ///     }
    /// }
    /// ```
    #[inline]
    pub unsafe fn reserve(&self, n: usize) -> Option<(*mut T, usize)> {
        self.channel.rings[self.id].reserve(n)
    }

    /// Commit `n` elements that were written after `reserve()`.
    #[inline]
    pub fn commit(&self, n: usize) {
        self.channel.rings[self.id].commit(n)
    }

    /// Push a single item. Returns `true` if successful.
    ///
    /// Returns `false` if the ring is full or closed.
    ///
    /// # Example
    ///
    /// ```ignore
    /// if !producer.push(42) {
    ///     // Handle backpressure
    /// }
    /// ```
    #[inline]
    pub fn push(&self, item: T) -> bool {
        // SAFETY: We write exactly 1 item and commit 1
        unsafe {
            if let Some((ptr, len)) = self.reserve(1) {
                debug_assert!(len >= 1);
                ptr.write(item);
                self.commit(1);
                true
            } else {
                false
            }
        }
    }

    /// Batch send items. Returns number sent.
    ///
    /// May send fewer items than provided if the ring fills up.
    #[inline]
    pub fn send(&self, items: &[T]) -> usize
    where
        T: Copy,
    {
        let mut sent = 0;
        while sent < items.len() {
            unsafe {
                if let Some((ptr, len)) = self.reserve(items.len() - sent) {
                    let to_copy = len.min(items.len() - sent);
                    std::ptr::copy_nonoverlapping(items.as_ptr().add(sent), ptr, to_copy);
                    self.commit(to_copy);
                    sent += to_copy;
                } else {
                    break; // Ring full
                }
            }
        }
        sent
    }

    /// Close this producer's ring.
    #[inline]
    pub fn close(&self) {
        self.channel.rings[self.id].close();
    }

    /// Returns true if this producer's ring is closed.
    #[inline]
    pub fn is_closed(&self) -> bool {
        self.channel.rings[self.id].is_closed()
    }

    /// Returns the number of items currently in this producer's ring.
    #[inline]
    pub fn len(&self) -> usize {
        self.channel.rings[self.id].len()
    }

    /// Returns true if this producer's ring is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.channel.rings[self.id].is_empty()
    }
}

// =============================================================================
// TYPE ALIASES FOR COMMON CONFIGURATIONS
// =============================================================================

/// 4K slots per ring, 4 producers (~135KB total for u64)
pub type StackChannel4K4P<T> = StackChannel<T, 4096, 4>;

/// 4K slots per ring, 8 producers (~270KB total for u64)
pub type StackChannel4K8P<T> = StackChannel<T, 4096, 8>;

/// 4K slots per ring, 16 producers (~530KB total for u64)
pub type StackChannel4K16P<T> = StackChannel<T, 4096, 16>;

/// 8K slots per ring, 4 producers (~265KB total for u64)
pub type StackChannel8K4P<T> = StackChannel<T, 8192, 4>;

/// 16K slots per ring, 4 producers (~530KB total for u64)
pub type StackChannel16K4P<T> = StackChannel<T, 16384, 4>;

// =============================================================================
// TESTS
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_single_producer() {
        let channel: StackChannel<u64, 16, 4> = StackChannel::new();
        let producer = channel.register().unwrap();

        assert!(producer.push(10));
        assert!(producer.push(20));
        assert!(producer.push(30));

        let mut values = Vec::new();
        let consumed = channel.consume_all(|v| values.push(*v));

        assert_eq!(consumed, 3);
        assert_eq!(values, vec![10, 20, 30]);
    }

    #[test]
    fn test_multi_producer() {
        let channel: StackChannel<u64, 16, 4> = StackChannel::new();

        let p1 = channel.register().unwrap();
        let p2 = channel.register().unwrap();
        let p3 = channel.register().unwrap();

        assert_eq!(p1.id(), 0);
        assert_eq!(p2.id(), 1);
        assert_eq!(p3.id(), 2);

        p1.push(100);
        p2.push(200);
        p3.push(300);

        let mut sum = 0u64;
        let consumed = channel.consume_all(|v| sum += v);

        assert_eq!(consumed, 3);
        assert_eq!(sum, 600);
    }

    #[test]
    fn test_too_many_producers() {
        let channel: StackChannel<u64, 16, 2> = StackChannel::new();

        let _p1 = channel.register().unwrap();
        let _p2 = channel.register().unwrap();

        // Third should fail
        let result = channel.register();
        assert!(matches!(
            result,
            Err(StackChannelError::TooManyProducers { max: 2 })
        ));
    }

    #[test]
    fn test_closed_channel() {
        let channel: StackChannel<u64, 16, 4> = StackChannel::new();

        let producer = channel.register().unwrap();
        assert!(producer.push(42));

        channel.close();

        // New registration should fail
        assert!(matches!(
            channel.register(),
            Err(StackChannelError::Closed)
        ));

        // Existing producer can't push
        assert!(!producer.push(99));

        // But we can still consume existing items
        let mut values = Vec::new();
        channel.consume_all(|v| values.push(*v));
        assert_eq!(values, vec![42]);
    }

    #[test]
    fn test_batch_send() {
        let channel: StackChannel<u64, 16, 4> = StackChannel::new();
        let producer = channel.register().unwrap();

        let items = [1, 2, 3, 4, 5];
        let sent = producer.send(&items);
        assert_eq!(sent, 5);

        let mut received = Vec::new();
        channel.consume_all(|v| received.push(*v));
        assert_eq!(received, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_consume_all_owned() {
        let channel: StackChannel<String, 16, 4> = StackChannel::new();
        let producer = channel.register().unwrap();

        producer.push("hello".to_string());
        producer.push("world".to_string());

        let mut owned: Vec<String> = Vec::new();
        channel.consume_all_owned(|s| owned.push(s));

        assert_eq!(owned, vec!["hello".to_string(), "world".to_string()]);
    }

    #[test]
    fn test_consume_up_to() {
        let channel: StackChannel<u64, 16, 4> = StackChannel::new();

        let p1 = channel.register().unwrap();
        let p2 = channel.register().unwrap();

        // p1 sends 3, p2 sends 3
        p1.send(&[1, 2, 3]);
        p2.send(&[4, 5, 6]);

        // Consume at most 4
        let mut sum = 0u64;
        let consumed = channel.consume_all_up_to(4, |v| sum += v);

        assert_eq!(consumed, 4);
        // Should get all 3 from p1 (1+2+3=6) + 1 from p2 (4) = 10
        assert_eq!(sum, 10);
    }

    #[test]
    fn test_producer_count() {
        let channel: StackChannel<u64, 16, 4> = StackChannel::new();

        assert_eq!(channel.producer_count(), 0);

        let _p1 = channel.register().unwrap();
        assert_eq!(channel.producer_count(), 1);

        let _p2 = channel.register().unwrap();
        assert_eq!(channel.producer_count(), 2);
    }

    #[test]
    fn test_get_ring() {
        let channel: StackChannel<u64, 16, 4> = StackChannel::new();

        // All rings accessible even before registration
        assert!(channel.get_ring(0).is_some());
        assert!(channel.get_ring(3).is_some());
        assert!(channel.get_ring(4).is_none()); // Out of bounds
    }
}
