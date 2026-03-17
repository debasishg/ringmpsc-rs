//! Tests for custom allocator integration.
//!
//! Run with: `cargo test -p ringmpsc-rs --test allocator_tests --release`

use ringmpsc_rs::{AlignedAllocator, BufferAllocator, Channel, Config, HeapAllocator, Ring};
use std::mem::MaybeUninit;
use std::ops::{Deref, DerefMut};

// -------------------------------------------------------
// Test 1: HeapAllocator basic correctness (explicit type)
// -------------------------------------------------------

#[test]
fn test_heap_allocator_ring() {
    let ring = Ring::<u64>::new(Config::default());
    assert!(ring.push(42));
    let mut val = 0u64;
    ring.consume_batch(|item| val = *item);
    assert_eq!(val, 42);
}

#[test]
fn test_heap_allocator_ring_explicit() {
    let ring = Ring::<u64, HeapAllocator>::new_in(Config::default(), HeapAllocator);
    assert!(ring.push(42));
    let mut val = 0u64;
    ring.consume_batch(|item| val = *item);
    assert_eq!(val, 42);
}

#[test]
fn test_heap_allocator_channel() {
    let ch = Channel::<u64>::new(Config::default());
    let p = ch.register().unwrap();
    p.push(99);
    let mut val = 0u64;
    ch.consume_all(|item| val = *item);
    assert_eq!(val, 99);
}

#[test]
fn test_heap_allocator_channel_explicit() {
    let ch = Channel::<u64, HeapAllocator>::new_in(Config::default(), HeapAllocator);
    let p = ch.register().unwrap();
    p.push(99);
    let mut val = 0u64;
    ch.consume_all(|item| val = *item);
    assert_eq!(val, 99);
}

// -------------------------------------------------------
// Test 2: Custom stable allocator (Vec-backed buffer)
// -------------------------------------------------------

/// A custom allocator for testing. Uses Vec internally but demonstrates
/// that the `BufferAllocator` trait works with non-Box buffer types.
#[derive(Clone, Copy, Debug, Default)]
struct VecAllocator;

/// Buffer wrapper backed by Vec that implements Deref/DerefMut.
struct VecBuffer<T> {
    inner: Vec<MaybeUninit<T>>,
}

impl<T> Deref for VecBuffer<T> {
    type Target = [MaybeUninit<T>];
    fn deref(&self) -> &[MaybeUninit<T>] {
        &self.inner
    }
}

impl<T> DerefMut for VecBuffer<T> {
    fn deref_mut(&mut self) -> &mut [MaybeUninit<T>] {
        &mut self.inner
    }
}

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

#[test]
fn test_custom_allocator_ring_basic() {
    let ring = Ring::new_in(Config::default(), VecAllocator);
    assert!(ring.push(100u64));
    assert!(ring.push(200u64));
    let mut sum = 0u64;
    ring.consume_batch(|item| sum += *item);
    assert_eq!(sum, 300);
}

#[test]
fn test_custom_allocator_ring_reserve_commit() {
    let ring = Ring::new_in(Config::default(), VecAllocator);

    if let Some(mut r) = ring.reserve(4) {
        let slice = r.as_mut_slice();
        slice[0].write(10u64);
        slice[1].write(20u64);
        slice[2].write(30u64);
        slice[3].write(40u64);
        r.commit();
    }

    assert_eq!(ring.len(), 4);

    if let Some(slice) = ring.readable() {
        assert_eq!(slice[0], 10);
        assert_eq!(slice[3], 40);
        ring.advance(4);
    }

    assert!(ring.is_empty());
}

#[test]
fn test_custom_allocator_channel_multi_producer() {
    let ch = Channel::new_in(Config::default(), VecAllocator);
    let p1 = ch.register().unwrap();
    let p2 = ch.register().unwrap();
    p1.push(10u64);
    p2.push(20u64);
    let mut sum = 0u64;
    ch.consume_all(|item| sum += *item);
    assert_eq!(sum, 30);
}

#[test]
fn test_custom_allocator_wrap_around() {
    let config = Config::new(2, 4, false); // ring_bits=2 → capacity = 4
    let ring = Ring::new_in(config, VecAllocator);

    for round in 0..5u64 {
        for i in 0..4u64 {
            assert!(ring.push(round * 10 + i));
        }
        let mut count = 0;
        ring.consume_batch(|_| count += 1);
        assert_eq!(count, 4);
    }
    assert!(ring.is_empty());
}

#[test]
fn test_custom_allocator_drop_behavior() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

    struct DropTracker(#[allow(dead_code)] u64);
    impl Drop for DropTracker {
        fn drop(&mut self) {
            DROP_COUNT.fetch_add(1, Ordering::SeqCst);
        }
    }

    DROP_COUNT.store(0, Ordering::SeqCst);

    let ring = Ring::new_in(Config::new(2, 4, false), VecAllocator);
    if let Some(mut r) = ring.reserve(3) {
        for (i, slot) in r.as_mut_slice().iter_mut().enumerate() {
            slot.write(DropTracker(i as u64));
        }
        r.commit();
    }

    // Consume 1, leave 2 unconsumed
    ring.consume_up_to(1, |_| {});
    assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 1);

    // Drop ring - should drop remaining 2
    drop(ring);
    assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 3);
}

#[test]
fn test_custom_allocator_concurrent_stress() {
    use std::sync::Arc;
    use std::thread;

    let channel = Arc::new(Channel::new_in(Config::default(), VecAllocator));
    let msg_count = 50_000u64;

    // Spawn 4 producers
    let mut handles = vec![];
    for producer_id in 0..4u32 {
        let ch = Arc::clone(&channel);
        handles.push(thread::spawn(move || {
            let producer = ch.register().unwrap();
            for i in 0..msg_count {
                while !producer.push(u64::from(producer_id) * 1_000_000 + i) {
                    std::hint::spin_loop();
                }
            }
        }));
    }

    // Consumer on main thread
    let expected = msg_count * 4;
    let mut total = 0u64;
    while total < expected {
        total += channel.consume_all(|_| {}) as u64;
        if total < expected {
            std::hint::spin_loop();
        }
    }

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(total, expected);
}

// -------------------------------------------------------
// Test 3: Nightly allocator-api bridge (conditional)
// -------------------------------------------------------

#[cfg(feature = "allocator-api")]
mod nightly_tests {
    use ringmpsc_rs::{Channel, Config, Ring, StdAllocator};

    #[test]
    fn test_std_allocator_global() {
        use std::alloc::Global;

        let alloc = StdAllocator(Global);
        let ring = Ring::new_in(Config::default(), alloc.clone());
        assert!(ring.push(42u64));
        let mut val = 0u64;
        ring.consume_batch(|item| val = *item);
        assert_eq!(val, 42);
    }

    #[test]
    fn test_std_allocator_channel() {
        use std::alloc::Global;

        let ch = Channel::new_in(Config::default(), StdAllocator(Global));
        let p = ch.register().unwrap();
        p.push(99u64);
        let mut val = 0u64;
        ch.consume_all(|item| val = *item);
        assert_eq!(val, 99);
    }
}

// -------------------------------------------------------
// Test 4: AlignedAllocator (128-byte cache-line alignment)
// -------------------------------------------------------

#[test]
fn test_aligned_allocator_128_ring_basic() {
    let ring = Ring::<u64, AlignedAllocator<128>>::new_in(Config::default(), AlignedAllocator::<128>);
    assert!(ring.push(42));
    assert!(ring.push(43));
    let mut sum = 0u64;
    ring.consume_batch(|item| sum += *item);
    assert_eq!(sum, 85);
}

#[test]
fn test_aligned_allocator_64_ring_basic() {
    let ring = Ring::<u64, AlignedAllocator<64>>::new_in(Config::default(), AlignedAllocator::<64>);
    assert!(ring.push(100));
    let mut val = 0u64;
    ring.consume_batch(|item| val = *item);
    assert_eq!(val, 100);
}

#[test]
fn test_aligned_allocator_buffer_is_actually_aligned() {
    // Verify at runtime that the buffer really is aligned to 128 bytes.
    let alloc = AlignedAllocator::<128>;
    let buf: <AlignedAllocator<128> as BufferAllocator>::Buffer<u64> = alloc.allocate(1024);
    let ptr = buf.as_ptr() as usize;
    assert_eq!(
        ptr % 128,
        0,
        "buffer pointer {:p} not 128-byte aligned",
        buf.as_ptr()
    );
}

#[test]
fn test_aligned_allocator_reserve_commit() {
    let ring = Ring::<u64, AlignedAllocator<128>>::new_in(Config::default(), AlignedAllocator::<128>);

    if let Some(mut r) = ring.reserve(4) {
        let slice = r.as_mut_slice();
        slice[0].write(10);
        slice[1].write(20);
        slice[2].write(30);
        slice[3].write(40);
        r.commit();
    }

    assert_eq!(ring.len(), 4);

    if let Some(slice) = ring.readable() {
        assert_eq!(slice[0], 10);
        assert_eq!(slice[3], 40);
        ring.advance(4);
    }
    assert!(ring.is_empty());
}

#[test]
fn test_aligned_allocator_channel_multi_producer() {
    let ch = Channel::new_in(Config::default(), AlignedAllocator::<128>);
    let p1 = ch.register().unwrap();
    let p2 = ch.register().unwrap();
    p1.push(10u64);
    p2.push(20u64);
    let mut sum = 0u64;
    ch.consume_all(|item| sum += *item);
    assert_eq!(sum, 30);
}

#[test]
fn test_aligned_allocator_wrap_around() {
    let config = Config::new(2, 4, false); // capacity = 4
    let ring = Ring::new_in(config, AlignedAllocator::<128>);

    for round in 0..5u64 {
        for i in 0..4u64 {
            assert!(ring.push(round * 10 + i));
        }
        let mut count = 0;
        ring.consume_batch(|_| count += 1);
        assert_eq!(count, 4);
    }
    assert!(ring.is_empty());
}

#[test]
fn test_aligned_allocator_drop_behavior() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

    struct DropTracker;
    impl Drop for DropTracker {
        fn drop(&mut self) {
            DROP_COUNT.fetch_add(1, Ordering::SeqCst);
        }
    }

    DROP_COUNT.store(0, Ordering::SeqCst);

    let ring = Ring::new_in(Config::new(2, 4, false), AlignedAllocator::<128>);
    if let Some(mut r) = ring.reserve(3) {
        for slot in r.as_mut_slice().iter_mut() {
            slot.write(DropTracker);
        }
        r.commit();
    }

    // Consume 1, leave 2 unconsumed
    ring.consume_up_to(1, |_| {});
    assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 1);

    // Drop ring — should drop remaining 2
    drop(ring);
    assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 3);
}

#[test]
fn test_aligned_allocator_concurrent_stress() {
    use std::sync::Arc;
    use std::thread;

    let channel = Arc::new(Channel::new_in(Config::default(), AlignedAllocator::<128>));
    let msg_count = 50_000u64;

    let mut handles = vec![];
    for producer_id in 0..4u32 {
        let ch = Arc::clone(&channel);
        handles.push(thread::spawn(move || {
            let producer = ch.register().unwrap();
            for i in 0..msg_count {
                while !producer.push(u64::from(producer_id) * 1_000_000 + i) {
                    std::hint::spin_loop();
                }
            }
        }));
    }

    let expected = msg_count * 4;
    let mut total = 0u64;
    while total < expected {
        total += channel.consume_all(|_| {}) as u64;
        if total < expected {
            std::hint::spin_loop();
        }
    }

    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(total, expected);
}

// -------------------------------------------------------
// Test 5: Bumpalo arena allocator
// -------------------------------------------------------

/// Demonstrates using a `bumpalo::Bump` arena as a ring buffer allocator.
///
/// The arena is leaked intentionally — bumpalo arenas free all memory at
/// once when they're dropped, which conflicts with the ring buffer's
/// assumption that individual buffers manage their own deallocation. By
/// leaking the arena reference we ensure the backing memory lives long
/// enough. In real applications you'd tie arena lifetime to a request or
/// phase so the bulk-free happens at a known safe point.
mod bumpalo_tests {
    use super::*;
    use bumpalo::Bump;
    use std::sync::Arc;

    /// Thin wrapper around `&'static Bump` that implements `BufferAllocator`.
    ///
    /// # Safety
    ///
    /// Bumpalo's `Bump` is not `Sync` because its internal bookkeeping uses
    /// `Cell`. We implement `Sync` here because `BufferAllocator::allocate()`
    /// is only called during `Ring::new_in()` / `Channel::new_in()` — before
    /// the ring is shared across threads. The allocator is never invoked
    /// concurrently.
    #[derive(Clone, Copy)]
    struct BumpAllocator {
        bump: &'static Bump,
    }

    // Safety: allocate() is called only during single-threaded construction.
    // The resulting buffer is Send and does not reference the Bump after creation.
    unsafe impl Send for BumpAllocator {}
    unsafe impl Sync for BumpAllocator {}

    /// Buffer backed by a bumpalo arena allocation. Since the arena owns
    /// the memory, `Drop` is a no-op — deallocation happens when the
    /// arena itself is dropped.
    struct BumpBuffer<T> {
        ptr: *mut MaybeUninit<T>,
        len: usize,
    }

    // Safety: BumpBuffer owns exclusive access to its allocation.
    unsafe impl<T: Send> Send for BumpBuffer<T> {}

    impl<T> Deref for BumpBuffer<T> {
        type Target = [MaybeUninit<T>];
        fn deref(&self) -> &[MaybeUninit<T>] {
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }
    }

    impl<T> DerefMut for BumpBuffer<T> {
        fn deref_mut(&mut self) -> &mut [MaybeUninit<T>] {
            unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
        }
    }

    // Safety: allocate() returns a buffer of exactly `capacity` elements.
    // The bumpalo arena guarantees valid, aligned memory for the requested layout.
    // Memory is freed when the arena is dropped (which outlives the buffer by
    // construction — we use a 'static reference).
    unsafe impl BufferAllocator for BumpAllocator {
        type Buffer<T> = BumpBuffer<T>;

        fn allocate<T>(&self, capacity: usize) -> BumpBuffer<T> {
            let slice = self.bump.alloc_slice_fill_with(capacity, |_| MaybeUninit::uninit());
            BumpBuffer {
                ptr: slice.as_mut_ptr(),
                len: capacity,
            }
        }
    }

    fn make_bump_allocator() -> BumpAllocator {
        // Leak the arena so it lives for 'static. In tests this is fine.
        let bump: &'static Bump = Box::leak(Box::new(Bump::new()));
        BumpAllocator { bump }
    }

    #[test]
    fn test_bumpalo_ring_basic() {
        let alloc = make_bump_allocator();
        let ring = Ring::<u64, BumpAllocator>::new_in(Config::default(), alloc);
        assert!(ring.push(42));
        assert!(ring.push(43));
        let mut sum = 0u64;
        ring.consume_batch(|item| sum += *item);
        assert_eq!(sum, 85);
    }

    #[test]
    fn test_bumpalo_ring_reserve_commit() {
        let alloc = make_bump_allocator();
        let ring = Ring::<u64, BumpAllocator>::new_in(Config::default(), alloc);

        if let Some(mut r) = ring.reserve(4) {
            let s = r.as_mut_slice();
            s[0].write(1);
            s[1].write(2);
            s[2].write(3);
            s[3].write(4);
            r.commit();
        }

        assert_eq!(ring.len(), 4);

        if let Some(slice) = ring.readable() {
            assert_eq!(slice[0], 1);
            assert_eq!(slice[3], 4);
            ring.advance(4);
        }
        assert!(ring.is_empty());
    }

    #[test]
    fn test_bumpalo_channel() {
        let alloc = make_bump_allocator();
        let ch = Channel::<u64, BumpAllocator>::new_in(Config::default(), alloc);
        let p1 = ch.register().unwrap();
        let p2 = ch.register().unwrap();
        p1.push(10u64);
        p2.push(20u64);
        let mut sum = 0u64;
        ch.consume_all(|item| sum += *item);
        assert_eq!(sum, 30);
    }

    #[test]
    fn test_bumpalo_concurrent() {
        let alloc = make_bump_allocator();
        let channel = Arc::new(Channel::<u64, BumpAllocator>::new_in(Config::default(), alloc));
        let msg_count = 10_000u64;

        let mut handles = vec![];
        for _ in 0..2u32 {
            let ch = Arc::clone(&channel);
            handles.push(std::thread::spawn(move || {
                let producer = ch.register().unwrap();
                for i in 0..msg_count {
                    while !producer.push(i) {
                        std::hint::spin_loop();
                    }
                }
            }));
        }

        let expected = msg_count * 2;
        let mut total = 0u64;
        while total < expected {
            total += channel.consume_all(|_| {}) as u64;
            if total < expected {
                std::hint::spin_loop();
            }
        }

        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(total, expected);
    }
}
