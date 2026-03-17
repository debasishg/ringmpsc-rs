//! Custom allocator example for `RingMPSC`.
//!
//! Demonstrates three allocator strategies:
//! 1. Default `HeapAllocator` (zero overhead, identical to `Ring::new()`)
//! 2. `AlignedAllocator<128>` (128-byte cache-line aligned)
//! 3. A hand-rolled `VecAllocator` (custom buffer type)
//!
//! Run with: `cargo run -p ringmpsc-rs --example custom_allocator --release`

use ringmpsc_rs::{AlignedAllocator, BufferAllocator, Channel, Config, HeapAllocator, Ring};
use std::mem::MaybeUninit;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

// ---------------------------------------------------------------------------
// Custom allocator: Vec-backed buffer (demonstrates the trait mechanism)
// ---------------------------------------------------------------------------

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

/// A custom allocator using Vec internally.
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

// ---------------------------------------------------------------------------
// Benchmark helpers
// ---------------------------------------------------------------------------

const ITEMS: usize = 2_000_000;
const BATCH: usize = 1024;

fn bench_ring<A: BufferAllocator>(name: &str, ring: &Ring<u64, A>) {
    let start = Instant::now();

    let mut sent = 0usize;
    let mut received = 0usize;

    while received < ITEMS {
        // Write batches
        while sent < ITEMS && sent - received < 60_000 {
            let want = BATCH.min(ITEMS - sent);
            if let Some(mut r) = ring.reserve(want) {
                let len = r.as_mut_slice().len();
                for (i, slot) in r.as_mut_slice().iter_mut().enumerate() {
                    slot.write((sent + i) as u64);
                }
                r.commit();
                sent += len;
            } else {
                break;
            }
        }

        // Read batches
        received += ring.consume_batch(|_| {});
    }

    let elapsed = start.elapsed();
    let throughput = ITEMS as f64 / elapsed.as_secs_f64() / 1e6;
    println!("  {name:20} — {throughput:>8.1} M msg/sec  ({elapsed:.2?})");
}

fn bench_channel_mpsc<A: BufferAllocator + Clone + 'static>(name: &str, config: Config, alloc: A) {
    let channel = Arc::new(Channel::<u64, A>::new_in(config, alloc));
    let num_producers = 4;
    let per_producer = ITEMS / num_producers;
    let total = per_producer * num_producers;

    let start = Instant::now();

    let mut handles = vec![];
    for _ in 0..num_producers {
        let ch = Arc::clone(&channel);
        handles.push(thread::spawn(move || {
            let producer = ch.register().expect("register producer");
            let mut sent = 0;
            while sent < per_producer {
                let want = BATCH.min(per_producer - sent);
                if let Some(mut r) = producer.reserve(want) {
                    let len = r.as_mut_slice().len();
                    for (i, slot) in r.as_mut_slice().iter_mut().enumerate() {
                        slot.write((sent + i) as u64);
                    }
                    r.commit();
                    sent += len;
                } else {
                    std::hint::spin_loop();
                }
            }
        }));
    }

    let mut received = 0usize;
    while received < total {
        received += channel.consume_all(|_| {});
        if received < total {
            std::hint::spin_loop();
        }
    }

    for h in handles {
        h.join().unwrap();
    }

    let elapsed = start.elapsed();
    let throughput = total as f64 / elapsed.as_secs_f64() / 1e6;
    println!("  {name:20} — {throughput:>8.1} M msg/sec  ({elapsed:.2?})");
}

fn main() {
    println!("RingMPSC Custom Allocator Example");
    println!("==================================\n");

    let config = Config::default();

    // ---- Single-threaded SPSC (no contention) ----
    println!("Single-threaded SPSC ({ITEMS} msgs):");

    let heap_ring = Ring::<u64>::new(config);
    bench_ring("HeapAllocator", &heap_ring);

    let aligned_ring = Ring::<u64, AlignedAllocator<128>>::new_in(config, AlignedAllocator::<128>);
    bench_ring("AlignedAllocator<128>", &aligned_ring);

    let vec_ring = Ring::<u64, VecAllocator>::new_in(config, VecAllocator);
    bench_ring("VecAllocator", &vec_ring);

    // ---- Multi-threaded MPSC (4 producers) ----
    println!("\nMulti-threaded MPSC (4P × {} msgs):", ITEMS / 4);

    bench_channel_mpsc("HeapAllocator", config, HeapAllocator);
    bench_channel_mpsc("AlignedAllocator<128>", config, AlignedAllocator::<128>);
    bench_channel_mpsc("VecAllocator", config, VecAllocator);

    println!("\nDone.");
}
