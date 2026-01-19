use crate::{Config, Reservation, Ring};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use thiserror::Error;

/// Error types for channel operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ChannelError {
    /// Too many producers registered (exceeds max_producers config).
    #[error("too many producers registered (max: {max})")]
    TooManyProducers {
        /// The configured maximum number of producers.
        max: usize,
    },
    /// Channel is closed.
    #[error("channel is closed")]
    Closed,
}

/// Multi-Producer Single-Consumer channel using ring decomposition.
///
/// Each producer gets a dedicated SPSC ring, eliminating producer-producer contention.
pub struct Channel<T> {
    inner: Arc<ChannelInner<T>>,
}

struct ChannelInner<T> {
    rings: Vec<Ring<T>>,
    producer_count: AtomicUsize,
    closed: AtomicBool,
    config: Config,
}

impl<T> Channel<T> {
    /// Creates a new channel with the given configuration.
    pub fn new(config: Config) -> Self {
        let mut rings = Vec::with_capacity(config.max_producers);
        for _ in 0..config.max_producers {
            rings.push(Ring::new(config));
        }

        Self {
            inner: Arc::new(ChannelInner {
                rings,
                producer_count: AtomicUsize::new(0),
                closed: AtomicBool::new(false),
                config,
            }),
        }
    }

    /// Register a new producer. Returns an error if too many producers or closed.
    pub fn register(&self) -> Result<Producer<T>, ChannelError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(ChannelError::Closed);
        }

        let id = self.inner.producer_count.fetch_add(1, Ordering::SeqCst);
        if id >= self.inner.config.max_producers {
            self.inner.producer_count.fetch_sub(1, Ordering::SeqCst);
            return Err(ChannelError::TooManyProducers {
                max: self.inner.config.max_producers,
            });
        }

        self.inner.rings[id].set_active(true);

        Ok(Producer {
            channel: Arc::clone(&self.inner),
            id,
        })
    }

    /// Round-robin receive from all active producers (convenience method).
    pub fn recv(&self, out: &mut [T]) -> usize
    where
        T: Copy,
    {
        let mut total = 0;
        let count = self.inner.producer_count.load(Ordering::Acquire);

        for ring in &self.inner.rings[..count] {
            if total >= out.len() {
                break;
            }
            total += ring.recv(&mut out[total..]);
        }

        total
    }

    /// Batch consume from all producers - THE FAST PATH.
    ///
    /// Processes all available items from all rings with minimal atomic operations.
    pub fn consume_all<F>(&self, mut handler: F) -> usize
    where
        F: FnMut(&T),
    {
        let mut total = 0;
        let count = self.inner.producer_count.load(Ordering::Acquire);

        for ring in &self.inner.rings[..count] {
            total += ring.consume_batch(&mut handler);
        }

        total
    }

    /// Consume up to max_total items from all producers.
    ///
    /// Useful for real-world processing to limit batch size and avoid long pauses.
    /// Prefers earlier rings (producer 0, then 1, etc.).
    pub fn consume_all_up_to<F>(&self, max_total: usize, mut handler: F) -> usize
    where
        F: FnMut(&T),
    {
        let mut total = 0;
        let count = self.inner.producer_count.load(Ordering::Acquire);

        for ring in &self.inner.rings[..count] {
            if total >= max_total {
                break;
            }
            let remaining = max_total - total;
            total += ring.consume_up_to(remaining, &mut handler);
        }

        total
    }

    /// Close the channel, preventing further operations.
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
        let count = self.inner.producer_count.load(Ordering::Acquire);
        for ring in &self.inner.rings[..count] {
            ring.close();
        }
    }

    /// Returns true if the channel is closed.
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    /// Returns the number of registered producers.
    pub fn producer_count(&self) -> usize {
        self.inner.producer_count.load(Ordering::Acquire)
    }

    /// Get aggregated metrics snapshot from all rings if enabled.
    pub fn metrics(&self) -> crate::MetricsSnapshot {
        let mut m = crate::MetricsSnapshot::default();
        let count = self.inner.producer_count.load(Ordering::Acquire);

        for ring in &self.inner.rings[..count] {
            let rm = ring.metrics();
            m.messages_sent += rm.messages_sent;
            m.messages_received += rm.messages_received;
            m.batches_sent += rm.batches_sent;
            m.batches_received += rm.batches_received;
            m.reserve_spins += rm.reserve_spins;
        }

        m
    }

    /// Get a reference to a specific ring for dedicated consumer access.
    ///
    /// This allows implementing the N-producer N-consumer pattern where each
    /// consumer has a dedicated ring to read from (matching the Zig implementation).
    ///
    /// Returns None if the ring_id is >= max_producers.
    pub fn get_ring(&self, ring_id: usize) -> Option<&Ring<T>> {
        if ring_id < self.inner.config.max_producers {
            Some(&self.inner.rings[ring_id])
        } else {
            None
        }
    }
}

impl<T> Clone for Channel<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

// Safety: Channel is Send + Sync as long as T is Send
unsafe impl<T: Send> Send for Channel<T> {}
unsafe impl<T: Send> Sync for Channel<T> {}

/// Producer handle for sending to the channel.
///
/// Each producer has a dedicated ring buffer, eliminating contention.
pub struct Producer<T> {
    channel: Arc<ChannelInner<T>>,
    id: usize,
}

impl<T> Producer<T> {
    /// Get the producer's ID.
    #[inline]
    pub fn id(&self) -> usize {
        self.id
    }

    /// Reserve n slots for zero-copy writing. Returns None if full/closed.
    ///
    /// **Important:** The returned `Reservation` may contain **fewer than n items**
    /// if the reservation wraps around the ring buffer. Always check the slice length.
    /// See [`Ring::reserve`] for details and examples.
    #[inline]
    pub fn reserve(&self, n: usize) -> Option<Reservation<'_, T>> {
        self.channel.rings[self.id].reserve(n)
    }

    /// Reserve with adaptive backoff. Spins, yields, then gives up.
    #[inline]
    pub fn reserve_with_backoff(&self, n: usize) -> Option<Reservation<'_, T>> {
        self.channel.rings[self.id].reserve_with_backoff(n)
    }

    /// Send a single item (convenience).
    ///
    /// Returns `true` if the item was successfully enqueued, `false` if the
    /// ring is full or closed. This is the simplest API for single-item sends.
    ///
    /// # Example
    /// ```ignore
    /// let producer = channel.register().unwrap();
    /// if !producer.push(42) {
    ///     // Ring is full, handle backpressure
    /// }
    /// ```
    #[inline]
    pub fn push(&self, item: T) -> bool {
        self.channel.rings[self.id].push(item)
    }

    /// Batch send (convenience).
    #[inline]
    pub fn send(&self, items: &[T]) -> usize
    where
        T: Copy,
    {
        self.channel.rings[self.id].send(items)
    }

    /// Close the producer's ring.
    #[inline]
    pub fn close(&self) {
        self.channel.rings[self.id].close();
    }

    /// Returns true if the producer's ring is closed.
    #[inline]
    pub fn is_closed(&self) -> bool {
        self.channel.rings[self.id].is_closed()
    }
}

// Note: Producer intentionally does NOT implement Clone.
// Cloning would allow multiple threads to write to the same Ring,
// breaking the single-producer invariant that enables lock-free operation.

// Safety: Producer is Send + Sync as long as T is Send
unsafe impl<T: Send> Send for Producer<T> {}
unsafe impl<T: Send> Sync for Producer<T> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_multi_producer() {
        let ch = Channel::<u64>::new(Config::default());

        let p1 = ch.register().unwrap();
        let p2 = ch.register().unwrap();

        assert_eq!(p1.send(&[10, 11]), 2);
        assert_eq!(p2.send(&[20, 21]), 2);

        let mut out = [0u64; 10];
        let n = ch.recv(&mut out);
        assert_eq!(n, 4);
    }

    #[test]
    fn test_channel_consume_all() {
        let ch = Channel::<u64>::new(Config::default());

        let p1 = ch.register().unwrap();
        let p2 = ch.register().unwrap();

        assert_eq!(p1.send(&[1, 2, 3]), 3);
        assert_eq!(p2.send(&[4, 5, 6]), 3);

        let mut sum = 0u64;
        let consumed = ch.consume_all(|item| sum += item);

        assert_eq!(consumed, 6);
        assert_eq!(sum, 21);
    }

    #[test]
    fn test_channel_consume_up_to() {
        let ch = Channel::<u64>::new(Config::default());

        let p1 = ch.register().unwrap();
        let p2 = ch.register().unwrap();

        assert_eq!(p1.send(&[1, 2, 3]), 3);
        assert_eq!(p2.send(&[4, 5, 6]), 3);

        let mut sum = 0u64;
        let consumed = ch.consume_all_up_to(4, |item| sum += item);

        assert_eq!(consumed, 4);
        // Depending on order, but since p1 first: 1+2+3+4=10
        assert!(sum >= 10);
    }

    #[test]
    fn test_channel_too_many_producers() {
        let config = Config::new(16, 2, false); // max 2 producers
        let ch = Channel::<u64>::new(config);

        let _p1 = ch.register().unwrap();
        let _p2 = ch.register().unwrap();

        // Should fail
        assert!(matches!(ch.register(), Err(ChannelError::TooManyProducers { max: 2 })));
    }

    #[test]
    fn test_channel_closed() {
        let ch = Channel::<u64>::new(Config::default());
        ch.close();

        assert!(matches!(ch.register(), Err(ChannelError::Closed)));
    }
}
