#[cfg(debug_assertions)]
use crate::invariants::debug_assert_fifo_count;
use crate::allocator::{BufferAllocator, HeapAllocator};
use crate::{Config, Reservation, Ring};
use crate::registry::{ClaimError, Registry};
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(debug_assertions)]
use std::sync::atomic::AtomicU64;
use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::Arc;
use thiserror::Error;

/// Error types for channel operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ChannelError {
    /// Too many producers registered (exceeds `max_producers` config).
    #[error("too many producers registered (max: {max})")]
    TooManyProducers {
        /// The configured maximum number of producers.
        max: usize,
    },
    /// Channel is closed.
    #[error("channel is closed")]
    Closed,
    /// A [`Consumer`] for this channel already exists.
    #[error("channel already has a consumer")]
    ConsumerTaken,
}

/// Multi-Producer Single-Consumer channel using ring decomposition.
///
/// Each producer gets a dedicated SPSC ring, eliminating producer-producer contention.
///
/// The allocator parameter `A` controls how each ring's backing buffer is allocated.
/// The default [`HeapAllocator`] is a zero-sized type that produces identical
/// code to the non-generic version. The channel's own `Vec` and `Arc` always use
/// the global allocator — only the ring buffers (the hot data path) use `A`.
pub struct Channel<T, A: BufferAllocator = HeapAllocator> {
    inner: Arc<ChannelInner<T, A>>,
}

struct ChannelInner<T, A: BufferAllocator = HeapAllocator> {
    rings: Vec<Ring<T, A>>,
    /// Producer count and closed bit in one word (INV-CH-04). Its bit stops
    /// registrations; `closed` below is what `is_closed()` reports.
    registry: Registry,
    /// Set by `close()` after every ring is closed, so `is_closed() == true`
    /// means no ring still accepts new writes (INV-CH-04).
    closed: AtomicBool,
    /// Set while a [`Consumer`] exists. At most one can, which is what makes
    /// the rings single-consumer.
    consumer_taken: AtomicBool,
    config: Config,
    /// Per-producer consumption count for FIFO verification (debug only)
    #[cfg(debug_assertions)]
    consumed_counts: Vec<AtomicU64>,
}

impl<T> Channel<T, HeapAllocator> {
    /// Creates a new channel with the given configuration.
    ///
    /// Uses the default heap allocator. This is API-compatible with the
    /// pre-allocator version.
    #[must_use] 
    pub fn new(config: Config) -> Self {
        Self::new_in(config, HeapAllocator)
    }
}

#[cfg(feature = "numa")]
impl<T> Channel<T, crate::numa::NumaAllocator> {
    /// Creates a new channel with NUMA-aware ring allocation.
    ///
    /// Convenience constructor equivalent to:
    /// ```ignore
    /// Channel::new_in(config, NumaAllocator::new(policy))
    /// ```
    ///
    /// On Linux, each ring buffer's backing memory is bound to a NUMA node
    /// according to the given [`NumaPolicy`](crate::numa::NumaPolicy).
    /// On non-Linux platforms, falls back to heap allocation.
    #[must_use]
    pub fn new_numa(config: Config, policy: crate::numa::NumaPolicy) -> Self {
        Self::new_in(config, crate::numa::NumaAllocator::new(policy))
    }
}

impl<T, A: BufferAllocator + Clone> Channel<T, A> {
    /// Creates a new channel with the given configuration and allocator.
    ///
    /// The allocator is cloned for each ring buffer (one per `max_producers`).
    /// For the default [`HeapAllocator`] (a ZST), cloning is free.
    pub fn new_in(config: Config, alloc: A) -> Self {
        let mut rings = Vec::with_capacity(config.max_producers);
        for _ in 0..config.max_producers {
            rings.push(Ring::new_in(config, alloc.clone()));
        }

        #[cfg(debug_assertions)]
        let consumed_counts = (0..config.max_producers)
            .map(|_| AtomicU64::new(0))
            .collect();

        Self {
            inner: Arc::new(ChannelInner {
                rings,
                registry: Registry::new(),
                closed: AtomicBool::new(false),
                consumer_taken: AtomicBool::new(false),
                config,
                #[cfg(debug_assertions)]
                consumed_counts,
            }),
        }
    }
}

impl<T, A: BufferAllocator> Channel<T, A> {
    /// Register a new producer. Returns an error if too many producers or closed.
    ///
    /// `close()` stops registrations first and sets the flag `is_closed()`
    /// reads last, after every ring is closed. So while a close is under way,
    /// `register()` can already fail with `Closed` although `is_closed()`
    /// still returns `false`: `Err(Closed)` does not imply `is_closed()`
    /// (INV-CH-04).
    pub fn register(&self) -> Result<Producer<T, A>, ChannelError> {
        let max = self.inner.config.max_producers;
        let id = self.inner.registry.claim(max).map_err(|e| match e {
            ClaimError::Closed => ChannelError::Closed,
            ClaimError::Full => ChannelError::TooManyProducers { max },
        })?;

        self.inner.rings[id].set_active(true);

        Ok(Producer {
            channel: Arc::clone(&self.inner),
            id,
            _not_sync: PhantomData,
        })
    }

    /// Claim the channel's consumer handle.
    ///
    /// All consumption goes through the returned [`Consumer`]. Only one can
    /// exist at a time: while it is alive, further calls return
    /// [`ChannelError::ConsumerTaken`]. Dropping it releases the claim, so
    /// another thread can take over consumption.
    ///
    /// Draining after [`close`](Self::close) is allowed, so this does not fail
    /// on a closed channel.
    ///
    /// ```
    /// use ringmpsc_rs::{Channel, ChannelError, Config};
    ///
    /// let channel = Channel::<u64>::new(Config::default());
    /// let consumer = channel.consumer().unwrap();
    /// assert_eq!(channel.consumer().err(), Some(ChannelError::ConsumerTaken));
    ///
    /// drop(consumer);
    /// assert!(channel.consumer().is_ok());
    /// ```
    pub fn consumer(&self) -> Result<Consumer<T, A>, ChannelError> {
        // Acquire pairs with the Release in `Consumer::drop`, so everything the
        // previous consumer did (its cached tail, its slot reads) happens
        // before this one starts.
        if self.inner.consumer_taken.swap(true, Ordering::Acquire) {
            return Err(ChannelError::ConsumerTaken);
        }
        Ok(Consumer {
            inner: Arc::clone(&self.inner),
        })
    }

    /// Close the channel.
    ///
    /// After this returns, `register()` fails with `Closed`, and every
    /// existing producer's ring is closed: a `reserve` / `push` / `send` that
    /// starts after `close()` returns fails. One already in progress on
    /// another thread may still commit, since `close()` doesn't wait for it,
    /// so drain for the last time only after the producers are done (joined
    /// or dropped). `is_closed()` turns true only after every ring is closed
    /// (INV-CH-04). Items committed before the close can still be consumed.
    pub fn close(&self) {
        // Every producer that registered got an id below `count`, and every
        // later `register()` fails (INV-CH-04), so this closes all their rings.
        let count = self.inner.registry.close();
        for ring in &self.inner.rings[..count] {
            ring.close();
        }
        // Release: a thread that sees `is_closed()` also sees every ring
        // closed.
        self.inner.closed.store(true, Ordering::Release);
    }

    /// Returns true if the channel is closed.
    ///
    /// True only once `close()` has closed every ring, so it can lag a
    /// `register()` that already failed with `Closed` (INV-CH-04).
    #[must_use] 
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    /// Returns the number of registered producers.
    ///
    /// Synchronizes with `register()`: a thread that sees `k` here also sees
    /// everything the first `k` registering threads did before registering.
    #[must_use] 
    pub fn producer_count(&self) -> usize {
        // Acquire: seeing `k` producers means seeing what each of them did
        // before `register()`.
        self.inner.registry.count_acquire()
    }

    /// Get aggregated metrics snapshot from all rings if enabled.
    #[must_use] 
    pub fn metrics(&self) -> crate::MetricsSnapshot {
        let mut m = crate::MetricsSnapshot::default();
        let count = self.inner.registry.count();

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

    /// Get a reference to a specific ring, for inspection.
    ///
    /// A `&Ring` can report `len`, `is_empty`, `is_closed` and metrics, but it
    /// cannot consume: every consumer method needs `&mut Ring`, and the only
    /// route to consume a channel's rings is its [`Consumer`].
    ///
    /// ```compile_fail,E0596
    /// use ringmpsc_rs::{Channel, Config};
    ///
    /// let channel = Channel::<u64>::new(Config::default());
    /// let ring = channel.get_ring(0).unwrap();
    /// ring.consume_batch(|_| {}); // ERROR: cannot borrow as mutable
    /// ```
    ///
    /// For one consumer per producer (the N-producer, N-consumer pattern),
    /// use N channels with one producer each.
    ///
    /// Returns None if the `ring_id` is >= `max_producers`.
    #[must_use] 
    pub fn get_ring(&self, ring_id: usize) -> Option<&Ring<T, A>> {
        if ring_id < self.inner.config.max_producers {
            Some(&self.inner.rings[ring_id])
        } else {
            None
        }
    }
}

impl<T, A: BufferAllocator> Clone for Channel<T, A> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

// Safety: Channel is Send + Sync as long as T is Send.
// BufferAllocator requires Send + Sync.
unsafe impl<T: Send, A: BufferAllocator> Send for Channel<T, A> {}
unsafe impl<T: Send, A: BufferAllocator> Sync for Channel<T, A> {}

/// The consuming end of a [`Channel`].
///
/// Obtained from [`Channel::consumer`]. A channel has at most one `Consumer`
/// at a time, and every method takes `&mut self`, so exactly one thread can
/// drain the rings. That is the "single consumer" in MPSC: a consume moves
/// items out of the ring before it publishes the new head, and two consumers
/// running at once would move out the same items.
///
/// `Consumer` is not `Clone`:
///
/// ```compile_fail,E0599
/// use ringmpsc_rs::{Channel, Config};
///
/// let channel = Channel::<u64>::new(Config::default());
/// let consumer = channel.consumer().unwrap();
/// let second = consumer.clone(); // ERROR: no method `clone`
/// ```
///
/// and a shared reference to it cannot consume:
///
/// ```compile_fail,E0596
/// use ringmpsc_rs::{Channel, Config};
///
/// let channel = Channel::<u64>::new(Config::default());
/// let consumer = channel.consumer().unwrap();
/// let shared = &consumer;
/// shared.consume_all(|_| {}); // ERROR: cannot borrow as mutable
/// ```
pub struct Consumer<T, A: BufferAllocator = HeapAllocator> {
    inner: Arc<ChannelInner<T, A>>,
}

impl<T, A: BufferAllocator> Consumer<T, A> {
    /// Round-robin receive from all active producers (convenience method).
    pub fn recv(&mut self, out: &mut [T]) -> usize
    where
        T: Copy,
    {
        let mut total = 0;
        let count = self.inner.registry.count();

        for ring in &self.inner.rings[..count] {
            if total >= out.len() {
                break;
            }
            // SAFETY: `&mut self` is exclusive and a channel has at most one
            // `Consumer`, so no other consumer call on this ring can be running.
            total += unsafe { ring.recv_shared(&mut out[total..]) };
        }

        total
    }

    /// Batch consume from all producers - THE FAST PATH.
    ///
    /// Processes all available items from all rings with minimal atomic operations.
    ///
    /// # When to Use
    ///
    /// Use this method when:
    /// - `T` is `Copy` (e.g., `u64`, `i32`) - no clone overhead
    /// - You only need to inspect or log items without storing them
    ///
    /// For types expensive to clone (containing `String`, `HashMap`, `Vec`),
    /// prefer [`consume_all_owned`] which transfers ownership directly.
    pub fn consume_all<F>(&mut self, mut handler: F) -> usize
    where
        F: FnMut(&T),
    {
        let mut total = 0;
        let count = self.inner.registry.count();

        for (_producer_id, ring) in self.inner.rings[..count].iter().enumerate() {
            // SAFETY: `&mut self` is exclusive and a channel has at most one
            // `Consumer`, so no other consumer call on this ring can be running.
            let consumed = unsafe { ring.consume_batch_shared(&mut handler) };

            // INV-CH-03: Verify per-producer FIFO by tracking cumulative count
            #[cfg(debug_assertions)]
            {
                let old_count = self.inner.consumed_counts[_producer_id].load(Ordering::Relaxed);
                let new_count = old_count + consumed as u64;
                debug_assert_fifo_count!(_producer_id, old_count, new_count);
                self.inner.consumed_counts[_producer_id].store(new_count, Ordering::Relaxed);
            }

            total += consumed;
        }

        total
    }

    /// Consume up to `max_total` items from all producers.
    ///
    /// Useful for real-world processing to limit batch size and avoid long pauses.
    /// Prefers earlier rings (producer 0, then 1, etc.).
    ///
    /// # When to Use
    ///
    /// Use this method when:
    /// - `T` is `Copy` (e.g., `u64`, `i32`) - no clone overhead
    /// - You only need to inspect or log items without storing them
    ///
    /// For types expensive to clone, prefer [`consume_all_up_to_owned`].
    pub fn consume_all_up_to<F>(&mut self, max_total: usize, mut handler: F) -> usize
    where
        F: FnMut(&T),
    {
        let mut total = 0;
        let count = self.inner.registry.count();

        for (_producer_id, ring) in self.inner.rings[..count].iter().enumerate() {
            if total >= max_total {
                break;
            }
            let remaining = max_total - total;
            // SAFETY: `&mut self` is exclusive and a channel has at most one
            // `Consumer`, so no other consumer call on this ring can be running.
            let consumed = unsafe { ring.consume_up_to_shared(remaining, &mut handler) };

            // INV-CH-03: Verify per-producer FIFO by tracking cumulative count
            #[cfg(debug_assertions)]
            {
                let old_count = self.inner.consumed_counts[_producer_id].load(Ordering::Relaxed);
                let new_count = old_count + consumed as u64;
                debug_assert_fifo_count!(_producer_id, old_count, new_count);
                self.inner.consumed_counts[_producer_id].store(new_count, Ordering::Relaxed);
            }

            total += consumed;
        }

        total
    }

    /// Batch consume from all producers, transferring ownership - THE FAST PATH.
    ///
    /// Similar to [`consume_all`], but the handler receives ownership of each item.
    ///
    /// # When to Use
    ///
    /// Use this method when:
    /// - `T` contains heap allocations (`String`, `HashMap`, `Vec`, `Box<_>`)
    /// - You need to move items into a collection or storage
    /// - You want to avoid clone overhead entirely
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Zero-copy transfer into batch processor
    /// consumer.consume_all_owned(|span| batch_processor.add(span));
    /// ```
    pub fn consume_all_owned<F>(&mut self, mut handler: F) -> usize
    where
        F: FnMut(T),
    {
        let mut total = 0;
        let count = self.inner.registry.count();

        for (_producer_id, ring) in self.inner.rings[..count].iter().enumerate() {
            // SAFETY: `&mut self` is exclusive and a channel has at most one
            // `Consumer`, so no other consumer call on this ring can be running.
            let consumed = unsafe { ring.consume_batch_owned_shared(&mut handler) };

            // INV-CH-03: Verify per-producer FIFO by tracking cumulative count
            #[cfg(debug_assertions)]
            {
                let old_count = self.inner.consumed_counts[_producer_id].load(Ordering::Relaxed);
                let new_count = old_count + consumed as u64;
                debug_assert_fifo_count!(_producer_id, old_count, new_count);
                self.inner.consumed_counts[_producer_id].store(new_count, Ordering::Relaxed);
            }

            total += consumed;
        }

        total
    }

    /// Consume up to `max_total` items from all producers, transferring ownership.
    ///
    /// Similar to `consume_all_up_to`, but the handler receives ownership of each
    /// item instead of a reference. This is more efficient when you need to move
    /// items (e.g., into a collection) since it avoids cloning.
    pub fn consume_all_up_to_owned<F>(&mut self, max_total: usize, mut handler: F) -> usize
    where
        F: FnMut(T),
    {
        let mut total = 0;
        let count = self.inner.registry.count();

        for (_producer_id, ring) in self.inner.rings[..count].iter().enumerate() {
            if total >= max_total {
                break;
            }
            let remaining = max_total - total;
            // SAFETY: `&mut self` is exclusive and a channel has at most one
            // `Consumer`, so no other consumer call on this ring can be running.
            let consumed = unsafe { ring.consume_up_to_owned_shared(remaining, &mut handler) };

            // INV-CH-03: Verify per-producer FIFO by tracking cumulative count
            #[cfg(debug_assertions)]
            {
                let old_count = self.inner.consumed_counts[_producer_id].load(Ordering::Relaxed);
                let new_count = old_count + consumed as u64;
                debug_assert_fifo_count!(_producer_id, old_count, new_count);
                self.inner.consumed_counts[_producer_id].store(new_count, Ordering::Relaxed);
            }

            total += consumed;
        }

        total
    }
}

impl<T, A: BufferAllocator> Drop for Consumer<T, A> {
    fn drop(&mut self) {
        // Release pairs with the Acquire in `Channel::consumer`.
        self.inner.consumer_taken.store(false, Ordering::Release);
    }
}

/// Producer handle for sending to the channel.
///
/// Each producer has a dedicated ring buffer, eliminating contention.
///
/// A `Producer` is `Send` but not `Sync`: it can move to another thread,
/// but it can't be shared between threads (INV-SW-01, INV-CH-01). `push`
/// and `send` take `&self`, so a shared `&Producer` would let two threads
/// write the same slot and store the same `tail`.
///
/// ```compile_fail,E0277
/// use ringmpsc_rs::{Channel, Config};
///
/// let channel = Channel::<u64>::new(Config::new(3, 1, false));
/// let producer = channel.register().expect("one producer");
/// std::thread::scope(|s| {
///     s.spawn(|| producer.push(1)); // ERROR: `Producer` is not `Sync`
///     s.spawn(|| producer.push(2));
/// });
/// ```
///
/// Moving it to another thread is fine:
///
/// ```
/// use ringmpsc_rs::{Channel, Config};
///
/// let channel = Channel::<u64>::new(Config::new(3, 1, false));
/// let producer = channel.register().expect("one producer");
/// std::thread::spawn(move || assert!(producer.push(1)))
///     .join()
///     .expect("producer thread");
/// ```
pub struct Producer<T, A: BufferAllocator = HeapAllocator> {
    channel: Arc<ChannelInner<T, A>>,
    id: usize,
    /// `Cell<()>` is `!Sync`, which makes `Producer` `!Sync` (INV-SW-01).
    _not_sync: PhantomData<Cell<()>>,
}

impl<T, A: BufferAllocator> Producer<T, A> {
    /// Get the producer's ID.
    #[inline]
    #[must_use] 
    pub fn id(&self) -> usize {
        self.id
    }

    /// Reserve n slots for zero-copy writing. Returns None if full/closed.
    ///
    /// **Important:** The returned `Reservation` may contain **fewer than n items**
    /// if the reservation wraps around the ring buffer. Always check the slice length.
    /// See [`Ring::reserve`] for details and examples.
    ///
    /// Takes `&mut self`, which is what makes "one outstanding reservation per
    /// producer" a compile-time property: a second `reserve` cannot borrow-check
    /// while the first `Reservation` is alive. Without it, both calls would see
    /// the same un-advanced cursor and alias `&mut` over the same slots.
    ///
    /// Two overlapping reservations are rejected by the compiler:
    ///
    /// ```compile_fail
    /// use ringmpsc_rs::{Channel, Config};
    ///
    /// let channel = Channel::<u64>::new(Config::new(3, 1, false));
    /// let mut producer = channel.register().unwrap();
    ///
    /// let r1 = producer.reserve(2).unwrap();
    /// let r2 = producer.reserve(2).unwrap(); // ERROR: second mutable borrow
    /// drop((r1, r2));
    /// ```
    ///
    /// Sequential reserve/commit is fine:
    ///
    /// ```
    /// use ringmpsc_rs::{Channel, Config};
    /// use std::mem::MaybeUninit;
    ///
    /// let channel = Channel::<u64>::new(Config::new(3, 1, false));
    /// let mut producer = channel.register().unwrap();
    ///
    /// let mut r = producer.reserve(2).unwrap();
    /// assert!(r.push(7).is_ok());
    /// assert!(r.push(8).is_ok());
    /// r.commit();
    ///
    /// let mut seen = Vec::new();
    /// channel.consumer().unwrap().consume_all(|v| seen.push(*v));
    /// assert_eq!(seen, vec![7, 8]);
    /// ```
    #[inline]
    #[must_use]
    pub fn reserve(&mut self, n: usize) -> Option<Reservation<'_, T, A>> {
        // SAFETY: `&mut self` is exclusive and a `Producer` is the only route
        // to its ring (ids are unique per `register()` and `Producer` is not
        // `Clone`; INV-CH-01), so no other reservation on that ring can be
        // outstanding. `Producer` is not `Sync`, so every producer call on the
        // ring stays on the thread that owns it (INV-SW-01).
        unsafe { self.channel.rings[self.id].reserve_shared(n) }
    }

    /// Reserve with adaptive backoff. Spins, yields, then gives up.
    ///
    /// Takes `&mut self` for the same reason as [`Producer::reserve`].
    #[inline]
    #[must_use]
    pub fn reserve_with_backoff(&mut self, n: usize) -> Option<Reservation<'_, T, A>> {
        // SAFETY: as in `reserve`: `&mut self` is exclusive, this is the
        // ring's only producer (INV-CH-01), and its calls stay on one thread
        // (INV-SW-01).
        unsafe { self.channel.rings[self.id].reserve_with_backoff_shared(n) }
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
        // SAFETY: `&self` suffices here because a `Reservation` can only be
        // obtained through `reserve(&mut self)`, and while one is alive it
        // holds that `&mut` borrow - so this method cannot be called. No
        // other thread can call it at the same time: `Producer` is not `Sync`,
        // so `&self` stays on one thread (INV-SW-01). No other route reaches
        // this ring: ids are unique per `register()`, `Producer` is not
        // `Clone`, and `Channel::get_ring` hands out `&Ring`, which can
        // neither reserve nor consume (INV-CH-01).
        unsafe { self.channel.rings[self.id].push_shared(item) }
    }

    /// Batch send (convenience).
    #[inline]
    pub fn send(&self, items: &[T]) -> usize
    where
        T: Copy,
    {
        // SAFETY: as in `push`: no `Reservation` can be alive while `&self` is
        // usable, this is the ring's only producer (INV-CH-01), and `Producer`
        // is not `Sync`, so its calls stay on one thread (INV-SW-01).
        unsafe { self.channel.rings[self.id].send_shared(items) }
    }

    /// Close the producer's ring.
    #[inline]
    pub fn close(&self) {
        self.channel.rings[self.id].close();
    }

    /// Returns true if the producer's ring is closed.
    #[inline]
    #[must_use] 
    pub fn is_closed(&self) -> bool {
        self.channel.rings[self.id].is_closed()
    }
}

// Note: Producer intentionally does NOT implement Clone or Sync.
// Either would let two threads write to the same Ring, breaking the
// single-producer invariant that enables lock-free operation.

// Safety: moving a Producer to another thread moves its exclusive claim on
// its ring with it, so the ring still has one producer (INV-SW-01,
// INV-CH-01). Items of type T cross threads, so T must be Send.
// BufferAllocator requires Send + Sync. Producer is deliberately not Sync.
unsafe impl<T: Send, A: BufferAllocator> Send for Producer<T, A> {}

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
        let n = ch.consumer().unwrap().recv(&mut out);
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
        let consumed = ch.consumer().unwrap().consume_all(|item| sum += item);

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
        let consumed = ch.consumer().unwrap().consume_all_up_to(4, |item| sum += item);

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
    fn test_consumer_is_exclusive() {
        let ch = Channel::<u64>::new(Config::default());
        let clone = ch.clone();

        let consumer = ch.consumer().unwrap();
        // A second claim fails, through the same handle or a clone of it.
        assert_eq!(ch.consumer().err(), Some(ChannelError::ConsumerTaken));
        assert_eq!(clone.consumer().err(), Some(ChannelError::ConsumerTaken));

        // Dropping the consumer releases the claim.
        drop(consumer);
        assert!(clone.consumer().is_ok());
    }

    #[test]
    fn test_consumer_hands_off_across_threads() {
        let ch = Channel::<String>::new(Config::new(4, 1, false));
        let p = ch.register().unwrap();
        for i in 0..3 {
            assert!(p.push(i.to_string()));
        }

        // The first consumer drains one item on another thread, then drops.
        let mut first = ch.consumer().unwrap();
        let taken = std::thread::spawn(move || {
            let mut got = Vec::new();
            first.consume_all_up_to_owned(1, |s| got.push(s));
            got
        })
        .join()
        .unwrap();
        assert_eq!(taken, vec!["0".to_string()]);

        // The next consumer resumes where it stopped: nothing is seen twice.
        let mut rest = Vec::new();
        ch.consumer().unwrap().consume_all_owned(|s| rest.push(s));
        assert_eq!(rest, vec!["1".to_string(), "2".to_string()]);
    }

    #[test]
    fn test_channel_closed() {
        let ch = Channel::<u64>::new(Config::default());
        ch.close();

        assert!(matches!(ch.register(), Err(ChannelError::Closed)));
    }
}
