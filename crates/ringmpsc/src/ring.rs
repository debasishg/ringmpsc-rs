use crate::allocator::{BufferAllocator, HeapAllocator};
use crate::invariants::{
    debug_assert_bounded_count, debug_assert_head_not_past_tail, debug_assert_initialized_read,
    debug_assert_monotonic, debug_assert_no_wrap,
};
use crate::{Backoff, Config, Metrics, Reservation};
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

// =============================================================================
// MEMORY ORDERING & SYNCHRONIZATION STRATEGY
// =============================================================================
//
// This SPSC ring buffer uses a classic producer-consumer protocol with the
// following synchronization guarantees:
//
// ## Sequence Numbers (ABA Prevention)
//
// We use unbounded u64 sequence numbers for `head` and `tail` instead of
// wrapped indices. This prevents the ABA problem entirely:
// - With 2^64 possible values, wrap-around is practically impossible
// - At 10 billion messages/second, wrap takes ~58 years
// - Buffer index is computed as `sequence & mask` only when accessing slots
//
// ## Memory Ordering Protocol
//
// **Producer (write path):**
// 1. Load `tail` with Relaxed (only producer writes tail)
// 2. Load `cached_head` with no ordering (UnsafeCell, single-writer)
// 3. If cache insufficient: Load `head` with Acquire (synchronizes with consumer)
// 4. Write data to buffer slots (no ordering needed - protected by protocol)
// 5. Store `tail` with Release (publishes writes to consumer)
//
// **Consumer (read path):**
// 1. Load `head` with Relaxed (only consumer writes head)
// 2. Load `cached_tail` with no ordering (UnsafeCell, single-writer)
// 3. If cache insufficient: Load `tail` with Acquire (synchronizes with producer)
// 4. Read data from buffer slots (no ordering needed - protected by protocol)
// 5. Store `head` with Release (publishes consumption to producer)
//
// ## Single-Writer Invariants
//
// The following fields are accessed via UnsafeCell without atomics because
// they have exactly one writer:
// - `cached_head`: Only written by producer, read by producer
// - `cached_tail`: Only written by consumer, read by consumer
// - `buffer[idx]`: Written by producer (between tail reservation and commit),
//                  read by consumer (between head load and advance)
//
// These invariants are enforced by the SPSC design: one Producer handle,
// one consumer (Channel polls rings sequentially on single thread).
//
// =============================================================================

/// SPSC ring buffer - the core building block.
///
/// A single-producer single-consumer ring buffer with lock-free operations.
/// Optimized with:
/// - 128-byte alignment to prevent false sharing
/// - Cached sequence numbers to minimize cross-core traffic
/// - Batch operations to amortize atomic overhead
///
/// The allocator parameter `A` controls how the backing buffer is allocated.
/// The default [`HeapAllocator`] is a zero-sized type that produces identical
/// code to the non-generic version.
#[repr(C)]
pub struct Ring<T, A: BufferAllocator = HeapAllocator> {
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
    /// Thread-safe metrics (uses atomics internally)
    metrics: Metrics,

    // === CONFIG ===
    config: Config,

    // === DATA BUFFER === (64-byte aligned)
    /// Raw pointer to slot 0 of the backing buffer.
    ///
    /// Derived exactly once in [`Ring::new_in`], before the ring is shared with
    /// any other thread, and never re-derived afterwards. Every concurrent
    /// access goes through this pointer instead of through `buffer`'s
    /// `Deref`/`DerefMut`.
    ///
    /// This matters for soundness, not just speed. `A::Buffer<T>` derefs to
    /// `[MaybeUninit<T>]` spanning the *whole* allocation, so `&*self.buffer.get()`
    /// retags every slot in the ring - including the slots the other side of the
    /// SPSC protocol is concurrently writing. A retag counts as an access for the
    /// purposes of the aliasing model, so that races with the producer's write
    /// even though the slots each side actually touches are disjoint. Miri
    /// reports it as a data race under both Stacked and Tree Borrows.
    base: *mut MaybeUninit<T>,

    /// The actual ring buffer storage, allocated via [`BufferAllocator`].
    ///
    /// For the default `HeapAllocator`, this is `Box<[MaybeUninit<T>]>` —
    /// identical layout and behavior to the pre-allocator version.
    ///
    /// Owns the allocation and frees it on drop. Not read on any concurrent
    /// path - use [`Ring::slot`] instead.
    ///
    /// The extra `Box` is load-bearing. Retagging a `Box` asserts uniqueness
    /// over its pointee, so every move of the `Ring` - returning it from
    /// `new_in`, pushing it into `ChannelInner::rings` - would retag a bare
    /// `A::Buffer<T>` held inline and invalidate `base`. Behind this `Box` the
    /// buffer handle has a fixed address and is never moved again, so `base`
    /// stays valid for the ring's whole life. Costs one allocation at
    /// construction and is never touched on a hot path.
    buffer: Box<UnsafeCell<A::Buffer<T>>>,
}

// Safety: Ring is Send + Sync as long as T is Send.
// The atomic operations ensure proper synchronization.
// BufferAllocator requires Send + Sync, and Buffer<T> requires Send.
unsafe impl<T: Send, A: BufferAllocator> Send for Ring<T, A> {}
unsafe impl<T: Send, A: BufferAllocator> Sync for Ring<T, A> {}

impl<T> Ring<T, HeapAllocator> {
    /// Creates a new ring buffer with the given configuration.
    ///
    /// Uses the default heap allocator (`Box<[MaybeUninit<T>]>`).
    /// This is API-compatible with the pre-allocator version.
    #[must_use] 
    pub fn new(config: Config) -> Self {
        Self::new_in(config, HeapAllocator)
    }
}

impl<T, A: BufferAllocator> Ring<T, A> {
    /// Creates a new ring buffer with the given configuration and allocator.
    ///
    /// The allocator is used to allocate the backing buffer at construction
    /// time. It is not stored in the ring — the buffer type handles its own
    /// deallocation on drop.
    pub fn new_in(config: Config, alloc: A) -> Self {
        let capacity = config.capacity();
        let buffer = Box::new(UnsafeCell::new(alloc.allocate::<T>(capacity)));

        // The one and only wide `&mut` over the buffer, taken while the ring is
        // still exclusively ours, so it races with nothing. `buffer` is already
        // in its final heap home, and the outer `Box` keeps it there, so this
        // pointer survives every later move of the `Ring`. See the field docs.
        let base = unsafe { (*buffer.get()).as_mut_ptr() };

        Self {
            base,
            tail: CacheAligned::new(AtomicU64::new(0)),
            cached_head: CacheAligned::new(UnsafeCell::new(0)),
            head: CacheAligned::new(AtomicU64::new(0)),
            cached_tail: CacheAligned::new(UnsafeCell::new(0)),
            active: CacheAligned::new(AtomicBool::new(false)),
            closed: AtomicBool::new(false),
            metrics: Metrics::new(),
            config,
            buffer,
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

    /// Raw pointer to slot `idx`.
    ///
    /// Forms no reference, so nothing is retagged beyond the single slot the
    /// caller goes on to touch. This is what keeps producer and consumer from
    /// racing on the buffer as a whole - see the docs on [`Ring::base`].
    ///
    /// # Safety
    ///
    /// `idx` must be less than `capacity()`. The caller must additionally hold
    /// the SPSC protocol's claim to slot `idx` (producer: `[tail, head+capacity)`,
    /// consumer: `[head, tail)`) before reading or writing through the result.
    #[inline]
    unsafe fn slot(&self, idx: usize) -> *mut MaybeUninit<T> {
        debug_assert!(idx < self.capacity(), "slot index {idx} out of bounds");
        // SAFETY: `base` points at `capacity()` contiguous slots and `idx` is
        // in bounds per the precondition.
        unsafe { self.base.add(idx) }
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
    ///
    /// # TLA+ Actions
    /// - Fast path: `ProducerReserveFast` (check `cached_head`)
    /// - Slow path: `ProducerRefreshCache` (Acquire load on head)
    ///
    /// Takes `&mut self`: a second `reserve` cannot borrow-check while the
    /// first `Reservation` is alive. That is load-bearing, not stylistic. The
    /// cursor does not advance until `commit()`, so two overlapping
    /// reservations would be handed `&mut` slices over the *same* slots.
    #[allow(clippy::cast_possible_truncation)]
    pub fn reserve(&mut self, n: usize) -> Option<Reservation<'_, T, A>> {
        // SAFETY: `&mut self` is exclusive, so no other reservation on this
        // ring can be outstanding.
        unsafe { self.reserve_shared(n) }
    }

    /// Reserve without proving exclusivity through the type system.
    ///
    /// # Safety
    ///
    /// No other `Reservation` for this ring may be outstanding when this is
    /// called, and none may be created until the returned one is committed or
    /// dropped. Callers holding a unique handle to the ring - `&mut Ring`, or
    /// `&mut Producer`, which is the only route to its ring - discharge this.
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) unsafe fn reserve_shared(&self, n: usize) -> Option<Reservation<'_, T, A>> {
        if n == 0 || n > self.capacity() {
            return None;
        }

        let tail = self.tail.load(Ordering::Relaxed);

        // Fast path: check cached head
        // SAFETY: cached_head is only written by the producer (this code path).
        // No other thread writes to it, so this unsynchronized read is safe.
        let cached_head = unsafe { *self.cached_head.get() };
        let space = self.capacity().saturating_sub(tail.wrapping_sub(cached_head) as usize);

        if space >= n {
            return Some(self.make_reservation(tail, n));
        }

        // Slow path: refresh cache
        let head = self.head.load(Ordering::Acquire);
        // SAFETY: cached_head is only written by the producer (this code path).
        // The Acquire load above synchronizes with the consumer's Release store.
        unsafe { *self.cached_head.get() = head; }

        let space = self.capacity().saturating_sub(tail.wrapping_sub(head) as usize);
        if space < n {
            return None;
        }

        Some(self.make_reservation(tail, n))
    }

    /// Reserve with adaptive backoff. Spins, yields, then gives up.
    ///
    /// Takes `&mut self` for the same reason as [`Ring::reserve`].
    pub fn reserve_with_backoff(&mut self, n: usize) -> Option<Reservation<'_, T, A>> {
        // SAFETY: `&mut self` is exclusive, so no other reservation on this
        // ring can be outstanding.
        unsafe { self.reserve_with_backoff_shared(n) }
    }

    /// Backoff reserve without proving exclusivity through the type system.
    ///
    /// # Safety
    ///
    /// Same obligation as [`Ring::reserve_shared`].
    pub(crate) unsafe fn reserve_with_backoff_shared(
        &self,
        n: usize,
    ) -> Option<Reservation<'_, T, A>> {
        let mut backoff = Backoff::new();
        while !backoff.is_completed() {
            // SAFETY: forwarded from this function's own precondition.
            if let Some(r) = unsafe { self.reserve_shared(n) } {
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
    fn make_reservation(&self, tail: u64, n: usize) -> Reservation<'_, T, A> {
        let mask = self.mask();
        let idx = (tail as usize) & mask;
        let contiguous = n.min(self.capacity() - idx);

        // SAFETY: Buffer access is safe because:
        // 1. idx is within bounds (masked to capacity)
        // 2. These slots are not being read by consumer (they're beyond current tail)
        // 3. Only the producer writes to slots between tail and tail+n
        // 4. The Reservation's commit() will publish via Release store to tail
        // 5. The slice spans exactly [idx, idx+contiguous), the producer's own
        //    region, so its retag cannot overlap slots the consumer holds.
        let slice = unsafe { std::slice::from_raw_parts_mut(self.slot(idx), contiguous) };

        // Create reservation with commit callback
        let ring_ptr = self as *const Self;
        Reservation::new(slice, ring_ptr)
    }

    /// Internal: Commit n slots after writing. Called by Reservation.
    ///
    /// # TLA+ Action: `ProducerWrite`
    /// Pre:  `(tail - head) < Capacity`
    /// Post: `tail' = tail + n`, data published via Release store
    pub(crate) fn commit_internal(&self, n: usize) {
        let tail = self.tail.load(Ordering::Relaxed);
        let new_tail = tail.wrapping_add(n as u64);
        let head = self.head.load(Ordering::Relaxed);

        // INV-SEQ-01: Bounded Count - items in ring never exceed capacity
        debug_assert_bounded_count!(new_tail.wrapping_sub(head) as usize, self.capacity());

        // INV-SEQ-02: Monotonic Progress - tail only increases
        debug_assert_monotonic!("tail", tail, new_tail);

        // INV-SEQ-03: No wrap-around (detects bugs, not real overflow)
        debug_assert_no_wrap!("tail", tail, new_tail);

        self.tail.store(new_tail, Ordering::Release);

        if self.config.enable_metrics {
            self.metrics.add_messages_sent(n as u64);
            self.metrics.add_batches_sent(1);
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
        // SAFETY: cached_tail is only written by the consumer (this code path).
        // No other thread writes to it, so this unsynchronized read is safe.
        let mut cached_tail = unsafe { *self.cached_tail.get() };
        let mut avail = cached_tail.wrapping_sub(head) as usize;

        if avail == 0 {
            // Slow path: refresh cache
            cached_tail = self.tail.load(Ordering::Acquire);
            // SAFETY: cached_tail is only written by the consumer (this code path).
            // The Acquire load above synchronizes with the producer's Release store.
            unsafe { *self.cached_tail.get() = cached_tail; }
            avail = cached_tail.wrapping_sub(head) as usize;
            if avail == 0 {
                return None;
            }
        }

        let mask = self.mask();
        let idx = (head as usize) & mask;
        let contiguous = avail.min(self.capacity() - idx);

        // SAFETY: Buffer access is safe because:
        // 1. idx is within bounds (masked to capacity)
        // 2. Items in [head, tail) were written by producer and published via Release
        // 3. The Acquire load on tail synchronizes with that Release
        // 4. Only consumer reads these slots; producer won't overwrite until head advances
        // 5. The slice spans exactly [idx, idx+contiguous), the consumer's own
        //    region, so its retag cannot overlap slots the producer is writing.
        unsafe {
            Some(std::slice::from_raw_parts(
                self.slot(idx).cast::<T>(),
                contiguous,
            ))
        }
    }

    /// Advance head after reading n items.
    ///
    /// # TLA+ Action: `ConsumerAdvance`
    /// Pre:  `head < tail` (items available)
    /// Post: `head' = head + n`, consumption published via Release store
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

        if self.config.enable_metrics {
            self.metrics.add_messages_received(n as u64);
            self.metrics.add_batches_received(1);
        }
    }

    // ---------------------------------------------------------------------
    // BATCH CONSUMPTION (Disruptor Pattern)
    // ---------------------------------------------------------------------
    //
    // Two variants are provided for each consumption method:
    //
    // 1. Reference variant (`consume_batch`, `consume_up_to`):
    //    - Handler receives `&T`
    //    - Use when: T is Copy, or you only need to inspect/log items
    //    - Example: `consume_batch(|item: &u64| sum += item)`
    //
    // 2. Owned variant (`consume_batch_owned`, `consume_up_to_owned`):
    //    - Handler receives `T` (ownership transferred)
    //    - Use when: T is expensive to clone (String, HashMap, Vec), or
    //      you need to move items into a collection
    //    - Example: `consume_batch_owned(|span| batch.push(span))`
    //
    // Both variants properly drop items after consumption. The owned variant
    // is more efficient when you need ownership since it avoids cloning.
    // ---------------------------------------------------------------------

    /// Process ALL available items with a single head update.
    ///
    /// This is the key optimization: amortizes atomic operations by processing
    /// the entire batch before updating the head pointer once.
    ///
    /// # TLA+ Actions
    /// - Cache refresh: `ConsumerRefreshCache` (Acquire load on tail)
    /// - Batch advance: `ConsumerAdvance` (Release store on head)
    ///
    /// # When to Use
    ///
    /// Use this method when:
    /// - `T` is `Copy` (e.g., `u64`, `i32`) - no clone overhead
    /// - You only need to inspect or log items without storing them
    /// - You want to aggregate values (sum, count, etc.)
    ///
    /// For types expensive to clone (containing `String`, `HashMap`, `Vec`),
    /// prefer [`consume_batch_owned`] which transfers ownership directly.
    ///
    /// # Drop Behavior
    ///
    /// Items are moved out of the buffer using `assume_init_read()` and dropped
    /// after the handler processes them. This ensures proper cleanup for types
    /// that implement `Drop`.
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
            // INV-INIT-01: Verify we're reading from initialized range
            debug_assert_initialized_read!(pos, head, tail);

            let idx = (pos as usize) & mask;
            // SAFETY: Buffer access is safe because:
            // 1. idx is within bounds (masked to capacity)
            // 2. Items in [head, tail) were fully written by producer
            // 3. The Acquire load on tail synchronizes with producer's Release store
            // 4. assume_init_read moves ownership out - item will be dropped after handler
            // 5. Only consumer reads these slots; after head advances, slots are "empty"
            //    and producer can reuse them
            // 6. Reading through `slot()` copies the one slot out without
            //    forming a reference that spans slots the producer owns.
            let item = unsafe { self.slot(idx).read().assume_init() };
            handler(&item);
            // `item` is dropped here, ensuring proper cleanup for T: Drop
            pos = pos.wrapping_add(1);
            count += 1;
        }

        // Single atomic update for entire batch
        self.head.store(tail, Ordering::Release);

        if self.config.enable_metrics {
            self.metrics.add_messages_received(count as u64);
            self.metrics.add_batches_received(1);
        }

        count
    }

    /// Process ALL available items with a single head update, transferring ownership.
    ///
    /// Similar to [`consume_batch`], but the handler receives ownership of each item
    /// instead of a reference.
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
    /// // Zero-copy transfer into a Vec
    /// let mut batch = Vec::new();
    /// ring.consume_batch_owned(|item| batch.push(item));
    /// ```
    ///
    /// # Drop Behavior
    ///
    /// The handler receives ownership and is responsible for the item. If the handler
    /// doesn't move or store the item, it will be dropped when the handler returns.
    #[allow(clippy::cast_possible_truncation)]
    pub fn consume_batch_owned<F>(&self, mut handler: F) -> usize
    where
        F: FnMut(T),
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
            // INV-INIT-01: Verify we're reading from initialized range
            debug_assert_initialized_read!(pos, head, tail);

            let idx = (pos as usize) & mask;
            // SAFETY: Buffer access is safe because:
            // 1. idx is within bounds (masked to capacity)
            // 2. Items in [head, tail) were fully written by producer
            // 3. The Acquire load on tail synchronizes with producer's Release store
            // 4. assume_init_read moves ownership out - handler takes ownership
            // 5. Only consumer reads these slots; after head advances, slots are "empty"
            // 6. Reading through `slot()` copies the one slot out without
            //    forming a reference that spans slots the producer owns.
            let item = unsafe { self.slot(idx).read().assume_init() };
            handler(item); // Transfer ownership to handler
            pos = pos.wrapping_add(1);
            count += 1;
        }

        // Single atomic update for entire batch
        self.head.store(tail, Ordering::Release);

        if self.config.enable_metrics {
            self.metrics.add_messages_received(count as u64);
            self.metrics.add_batches_received(1);
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
            // INV-INIT-01: Verify we're reading from initialized range
            debug_assert_initialized_read!(pos, head, tail);

            let idx = (pos as usize) & mask;
            // SAFETY: Buffer access is safe because:
            // 1. idx is within bounds (masked to capacity)
            // 2. Items in [head, tail) were fully written by producer
            // 3. The Acquire load on tail synchronizes with producer's Release store
            // 4. assume_init_read moves ownership out - item will be dropped after handler
            // 5. Only consumer reads these slots; after head advances, slots are "empty"
            //    and producer can reuse them
            // 6. Reading through `slot()` copies the one slot out without
            //    forming a reference that spans slots the producer owns.
            let item = unsafe { self.slot(idx).read().assume_init() };
            handler(&item);
            // `item` is dropped here, ensuring proper cleanup for T: Drop
            pos = pos.wrapping_add(1);
            count += 1;
        }

        // Single atomic update for the batch
        self.head.store(head.wrapping_add(count as u64), Ordering::Release);

        if self.config.enable_metrics {
            self.metrics.add_messages_received(count as u64);
            self.metrics.add_batches_received(1);
        }

        count
    }

    /// Consume up to `max_items` with a single head update, transferring ownership.
    ///
    /// Similar to `consume_up_to`, but the handler receives ownership of each item
    /// instead of a reference. This is more efficient when you need to move items
    /// (e.g., into a collection) since it avoids cloning.
    #[allow(clippy::cast_possible_truncation)]
    pub fn consume_up_to_owned<F>(&self, max_items: usize, mut handler: F) -> usize
    where
        F: FnMut(T),
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
            // INV-INIT-01: Verify we're reading from initialized range
            debug_assert_initialized_read!(pos, head, tail);

            let idx = (pos as usize) & mask;
            // SAFETY: Buffer access is safe because:
            // 1. idx is within bounds (masked to capacity)
            // 2. Items in [head, tail) were fully written by producer
            // 3. The Acquire load on tail synchronizes with producer's Release store
            // 4. assume_init_read moves ownership out - handler takes ownership
            // 5. Only consumer reads these slots; after head advances, slots are "empty"
            // 6. Reading through `slot()` copies the one slot out without
            //    forming a reference that spans slots the producer owns.
            let item = unsafe { self.slot(idx).read().assume_init() };
            handler(item); // Transfer ownership to handler
            pos = pos.wrapping_add(1);
            count += 1;
        }

        // Single atomic update for the batch
        self.head.store(head.wrapping_add(count as u64), Ordering::Release);

        if self.config.enable_metrics {
            self.metrics.add_messages_received(count as u64);
            self.metrics.add_batches_received(1);
        }

        count
    }

    // ---------------------------------------------------------------------
    // CONVENIENCE WRAPPERS
    // ---------------------------------------------------------------------

    /// Send a single item (convenience).
    ///
    /// Returns `true` if the item was successfully enqueued, `false` if the
    /// ring is full or closed. This is the simplest API for single-item sends.
    ///
    /// # Example
    /// ```ignore
    /// if !producer.push(42) {
    ///     // Ring is full, handle backpressure
    /// }
    /// ```
    #[inline]
    pub fn push(&mut self, item: T) -> bool {
        // SAFETY: `&mut self` is exclusive, so no reservation is outstanding.
        unsafe { self.push_shared(item) }
    }

    /// Push without proving exclusivity through the type system.
    ///
    /// # Safety
    ///
    /// Same obligation as [`Ring::reserve_shared`].
    #[inline]
    pub(crate) unsafe fn push_shared(&self, item: T) -> bool {
        // SAFETY: forwarded from this function's own precondition.
        unsafe { self.reserve_shared(1) }.is_some_and(|mut r| {
            r.as_mut_slice()[0] = std::mem::MaybeUninit::new(item);
            r.commit();
            true
        })
    }

    /// Batch send (convenience).
    pub fn send(&mut self, items: &[T]) -> usize
    where
        T: Copy,
    {
        // SAFETY: `&mut self` is exclusive, so no reservation is outstanding.
        unsafe { self.send_shared(items) }
    }

    /// Batch send without proving exclusivity through the type system.
    ///
    /// # Safety
    ///
    /// Same obligation as [`Ring::reserve_shared`].
    pub(crate) unsafe fn send_shared(&self, items: &[T]) -> usize
    where
        T: Copy,
    {
        // SAFETY: forwarded from this function's own precondition.
        unsafe { self.reserve_shared(items.len()) }.map_or(0, |mut reservation| {
            let slice = reservation.as_mut_slice();
            let n = slice.len();
            for i in 0..n {
                slice[i].write(items[i]);
            }
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

    /// Get a snapshot of metrics if enabled.
    pub fn metrics(&self) -> crate::MetricsSnapshot {
        if self.config.enable_metrics {
            self.metrics.snapshot()
        } else {
            crate::MetricsSnapshot::default()
        }
    }
}

impl<T, A: BufferAllocator> Drop for Ring<T, A> {
    fn drop(&mut self) {
        // Drop all initialized items in the ring
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Relaxed);
        let count = tail.wrapping_sub(head) as usize;

        if count > 0 {
            let capacity = self.capacity();
            let mask = self.mask();

            for i in 0..count {
                let idx = ((head as usize).wrapping_add(i)) & mask;
                // Safety: idx bounded by mask; slot in [head, tail) is initialized (INV-INIT-01, INV-DROP-01).
                // We hold `&mut self`, so there is no concurrent access, but we still
                // go through `slot()` rather than deref the buffer: a wide retag here
                // would be pointless and `base` is exactly the right pointer.
                crate::invariants::debug_assert_drop_bounds!(count, capacity, idx);
                unsafe {
                    ptr::drop_in_place((*self.slot(idx)).as_mut_ptr());
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
        let mut ring = Ring::<u64>::new(Config::default());

        // Write
        if let Some(mut r) = ring.reserve(4) {
            let slice = r.as_mut_slice();
            slice[0].write(100);
            slice[1].write(200);
            slice[2].write(300);
            slice[3].write(400);
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
        let mut ring = Ring::<u64>::new(Config::default());

        // Write 10 items
        for i in 0..10 {
            if let Some(mut r) = ring.reserve(1) {
                r.as_mut_slice()[0].write(i * 10);
                r.commit();
            }
        }

        // Consume all at once
        let mut sum = 0u64;
        let consumed = ring.consume_batch(|item| sum += item);

        assert_eq!(consumed, 10);
        assert_eq!(sum, 10 + 20 + 30 + 40 + 50 + 60 + 70 + 80 + 90);
        assert!(ring.is_empty());
    }

    #[test]
    fn test_ring_consume_up_to() {
        let mut ring = Ring::<u64>::new(Config::default());

        // Write 10 items
        for i in 0..10 {
            if let Some(mut r) = ring.reserve(1) {
                r.as_mut_slice()[0].write(i * 10);
                r.commit();
            }
        }

        // Consume up to 5
        let mut sum = 0u64;
        let consumed = ring.consume_up_to(5, |item| sum += item);

        assert_eq!(consumed, 5);
        assert_eq!(sum, 10 + 20 + 30 + 40);
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
        let mut ring = Ring::<u64>::new(config);

        // Fill it
        for i in 0..16 {
            if let Some(mut r) = ring.reserve(1) {
                r.as_mut_slice()[0].write(i);
                r.commit();
            }
        }

        // Should fail
        assert!(ring.reserve(1).is_none());
    }

    #[test]
    fn test_consume_batch_drops_items() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Counter for tracking drops
        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        #[derive(Clone)]
        struct DropTracker {
            _id: u64,
        }

        impl Drop for DropTracker {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, Ordering::SeqCst);
            }
        }

        DROP_COUNT.store(0, Ordering::SeqCst);

        let mut ring = Ring::<DropTracker>::new(Config::default());

        // Write 5 items
        for i in 0..5 {
            if let Some(mut r) = ring.reserve(1) {
                r.as_mut_slice()[0].write(DropTracker { _id: i });
                r.commit();
            }
        }

        assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 0);

        // Consume all - should drop each item after handler
        let consumed = ring.consume_batch(|_item| {
            // Item is still alive here
        });

        assert_eq!(consumed, 5);
        // All 5 items should have been dropped
        assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn test_consume_up_to_drops_items() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        struct DropTracker {
            _id: u64,
        }

        impl Drop for DropTracker {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, Ordering::SeqCst);
            }
        }

        DROP_COUNT.store(0, Ordering::SeqCst);

        let mut ring = Ring::<DropTracker>::new(Config::default());

        // Write 10 items
        for i in 0..10 {
            if let Some(mut r) = ring.reserve(1) {
                r.as_mut_slice()[0].write(DropTracker { _id: i });
                r.commit();
            }
        }

        // Consume 5 - should drop those 5
        let consumed = ring.consume_up_to(5, |_item| {});
        assert_eq!(consumed, 5);
        assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 5);

        // Consume remaining 5
        let consumed = ring.consume_up_to(10, |_item| {});
        assert_eq!(consumed, 5);
        assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 10);
    }
}
