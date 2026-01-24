//! Benchmark comparing StackRing (stack-allocated) vs Ring (heap-allocated)
//!
//! This benchmark measures SPSC (Single-Producer Single-Consumer) throughput
//! for both implementations to quantify the benefit of eliminating heap
//! pointer indirection.
//!
//! Also benchmarks StackChannel vs Channel for MPSC scenarios.
//!
//! Run with: cargo bench --features stack-ring --bench stack_vs_heap

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use ringmpsc_rs::{Channel, Config, Ring, StackChannel, StackRing};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

const MSG_COUNT: u64 = 5_000_000; // 5M messages per iteration
const BATCH_SIZE: usize = 1024;

// =============================================================================
// SINGLE-THREADED BENCHMARKS (no threading overhead)
// =============================================================================

/// Benchmark heap Ring - single threaded reserve/commit/consume cycle
fn bench_heap_single_thread(c: &mut Criterion) {
    let mut group = c.benchmark_group("single_thread");
    group.throughput(Throughput::Elements(MSG_COUNT));

    group.bench_function("heap_ring", |b| {
        let ring = Ring::<u32>::new(Config::default());
        
        b.iter(|| {
            let mut sent = 0u64;
            let mut received = 0u64;
            
            while received < MSG_COUNT {
                // Producer: write a batch
                while sent < MSG_COUNT && sent - received < 60000 {
                    let want = BATCH_SIZE.min((MSG_COUNT - sent) as usize);
                    if let Some(mut r) = ring.reserve(want) {
                        let len = r.as_mut_slice().len();
                        for (i, item) in r.as_mut_slice().iter_mut().enumerate() {
                            item.write(black_box((sent + i as u64) as u32));
                        }
                        r.commit();
                        sent += len as u64;
                    } else {
                        break;
                    }
                }
                
                // Consumer: drain
                let consumed = ring.consume_batch(|v| { black_box(v); });
                received += consumed as u64;
            }
            
            received
        });
    });

    group.finish();
}

/// Benchmark StackRing - single threaded reserve/commit/consume cycle
fn bench_stack_single_thread(c: &mut Criterion) {
    let mut group = c.benchmark_group("single_thread");
    group.throughput(Throughput::Elements(MSG_COUNT));

    group.bench_function("stack_ring", |b| {
        let ring: StackRing<u32, 65536> = StackRing::new();
        
        b.iter(|| {
            let mut sent = 0u64;
            let mut received = 0u64;
            
            while received < MSG_COUNT {
                // Producer: write a batch
                while sent < MSG_COUNT && sent - received < 60000 {
                    let want = BATCH_SIZE.min((MSG_COUNT - sent) as usize);
                    unsafe {
                        if let Some((ptr, len)) = ring.reserve(want) {
                            for i in 0..len {
                                *ptr.add(i) = black_box((sent + i as u64) as u32);
                            }
                            ring.commit(len);
                            sent += len as u64;
                        } else {
                            break;
                        }
                    }
                }
                
                // Consumer: drain
                let consumed = unsafe { ring.consume_batch(|v| { black_box(v); }) };
                received += consumed as u64;
            }
            
            received
        });
    });

    group.finish();
}

// =============================================================================
// MULTI-THREADED SPSC BENCHMARKS  
// =============================================================================

/// Benchmark heap Ring with producer/consumer threads
fn bench_heap_spsc(c: &mut Criterion) {
    let mut group = c.benchmark_group("spsc_throughput");
    group.throughput(Throughput::Elements(MSG_COUNT));

    group.bench_function("heap_ring", |b| {
        b.iter(|| {
            let ring = Arc::new(Ring::<u32>::new(Config::default()));
            let done = Arc::new(AtomicBool::new(false));

            let ring_p = Arc::clone(&ring);
            let done_p = Arc::clone(&done);
            
            let producer = thread::spawn(move || {
                let mut sent = 0u64;
                while sent < MSG_COUNT {
                    let want = BATCH_SIZE.min((MSG_COUNT - sent) as usize);
                    if let Some(mut r) = ring_p.reserve(want) {
                        let len = r.as_mut_slice().len();
                        for (i, item) in r.as_mut_slice().iter_mut().enumerate() {
                            item.write((sent + i as u64) as u32);
                        }
                        r.commit();
                        sent += len as u64;
                    } else {
                        std::hint::spin_loop();
                    }
                }
                done_p.store(true, Ordering::Release);
            });

            // Consumer
            let mut total = 0u64;
            loop {
                let consumed = ring.consume_batch(|_| {});
                total += consumed as u64;
                if consumed == 0 {
                    if done.load(Ordering::Acquire) && ring.is_empty() {
                        break;
                    }
                    std::hint::spin_loop();
                }
            }

            producer.join().unwrap();
            black_box(total)
        });
    });

    group.finish();
}

/// Benchmark StackRing with producer/consumer threads
fn bench_stack_spsc(c: &mut Criterion) {
    let mut group = c.benchmark_group("spsc_throughput");
    group.throughput(Throughput::Elements(MSG_COUNT));

    group.bench_function("stack_ring", |b| {
        b.iter(|| {
            // Allocate on heap but with inline buffer (no pointer indirection for data)
            let ring = Box::new(StackRing::<u32, 65536>::new());
            let ring_ptr = Box::into_raw(ring);
            
            // SAFETY: We control the lifetime - ring lives until both threads complete
            let ring_ref: &'static StackRing<u32, 65536> = unsafe { &*ring_ptr };
            
            let done = Arc::new(AtomicBool::new(false));
            let done_p = Arc::clone(&done);

            let producer = thread::spawn(move || {
                let mut sent = 0u64;
                while sent < MSG_COUNT {
                    let want = BATCH_SIZE.min((MSG_COUNT - sent) as usize);
                    unsafe {
                        if let Some((ptr, len)) = ring_ref.reserve(want) {
                            for i in 0..len {
                                *ptr.add(i) = (sent + i as u64) as u32;
                            }
                            ring_ref.commit(len);
                            sent += len as u64;
                        } else {
                            std::hint::spin_loop();
                        }
                    }
                }
                done_p.store(true, Ordering::Release);
            });

            // Consumer
            let mut total = 0u64;
            loop {
                let consumed = unsafe { ring_ref.consume_batch(|_| {}) };
                total += consumed as u64;
                if consumed == 0 {
                    if done.load(Ordering::Acquire) && ring_ref.is_empty() {
                        break;
                    }
                    std::hint::spin_loop();
                }
            }

            producer.join().unwrap();
            
            // Clean up
            unsafe { drop(Box::from_raw(ring_ptr)); }
            
            black_box(total)
        });
    });

    group.finish();
}

// =============================================================================
// DIFFERENT RING SIZES
// =============================================================================

fn bench_stack_sizes(c: &mut Criterion) {
    let mut group = c.benchmark_group("stack_ring_sizes");
    let msg_count = 2_000_000u64;
    group.throughput(Throughput::Elements(msg_count));

    // Macro to reduce repetition for different sizes
    macro_rules! bench_size {
        ($name:literal, $size:expr, $batch:expr) => {
            group.bench_function($name, |b| {
                let ring: StackRing<u32, $size> = StackRing::new();
                
                b.iter(|| {
                    let mut sent = 0u64;
                    let mut received = 0u64;
                    
                    while received < msg_count {
                        while sent < msg_count && sent - received < ($size - 100) as u64 {
                            let want = $batch.min((msg_count - sent) as usize);
                            unsafe {
                                if let Some((ptr, len)) = ring.reserve(want) {
                                    for i in 0..len {
                                        *ptr.add(i) = black_box((sent + i as u64) as u32);
                                    }
                                    ring.commit(len);
                                    sent += len as u64;
                                } else {
                                    break;
                                }
                            }
                        }
                        
                        let consumed = unsafe { ring.consume_batch(|v| { black_box(v); }) };
                        received += consumed as u64;
                    }
                    
                    received
                });
            });
        };
    }

    bench_size!("4K_slots", 4096, 256);
    bench_size!("8K_slots", 8192, 512);
    bench_size!("16K_slots", 16384, 1024);
    bench_size!("64K_slots", 65536, 1024);

    group.finish();
}

// =============================================================================
// MPSC BENCHMARKS: StackChannel vs Channel
// =============================================================================

const MPSC_MSG_COUNT: u64 = 2_000_000; // 2M messages total
const MPSC_BATCH: usize = 512;

/// Benchmark heap Channel with multiple producers
fn bench_heap_mpsc(c: &mut Criterion) {
    let mut group = c.benchmark_group("mpsc_throughput");
    
    for num_producers in [2usize, 4, 8] {
        let msg_per_producer = MPSC_MSG_COUNT / num_producers as u64;
        group.throughput(Throughput::Elements(MPSC_MSG_COUNT));
        
        group.bench_function(format!("heap_channel_{}P", num_producers), |b| {
            b.iter(|| {
                let config = Config::new(16, num_producers, false);
                let channel = Channel::<u32>::new(config);
                let done = Arc::new(AtomicBool::new(false));
                
                // Spawn producers
                let handles: Vec<_> = (0..num_producers)
                    .map(|_| {
                        let producer = channel.register().unwrap();
                        let _done = Arc::clone(&done);
                        thread::spawn(move || {
                            let mut sent = 0u64;
                            while sent < msg_per_producer {
                                let want = MPSC_BATCH.min((msg_per_producer - sent) as usize);
                                if let Some(mut r) = producer.reserve(want) {
                                    let len = r.as_mut_slice().len();
                                    for (i, item) in r.as_mut_slice().iter_mut().enumerate() {
                                        item.write((sent + i as u64) as u32);
                                    }
                                    r.commit();
                                    sent += len as u64;
                                } else {
                                    std::hint::spin_loop();
                                }
                            }
                        })
                    })
                    .collect();
                
                // Consumer
                let mut total = 0u64;
                loop {
                    let consumed = channel.consume_all(|_| {});
                    total += consumed as u64;
                    if total >= MPSC_MSG_COUNT {
                        break;
                    }
                    if consumed == 0 {
                        std::hint::spin_loop();
                    }
                }
                
                done.store(true, Ordering::Release);
                for h in handles {
                    h.join().unwrap();
                }
                
                black_box(total)
            });
        });
    }
    
    group.finish();
}

/// Benchmark StackChannel with multiple producers (2P)
fn bench_stack_mpsc_2p(c: &mut Criterion) {
    let mut group = c.benchmark_group("mpsc_throughput");
    group.throughput(Throughput::Elements(MPSC_MSG_COUNT));
    
    const NUM_PRODUCERS: usize = 2;
    let msg_per_producer = MPSC_MSG_COUNT / NUM_PRODUCERS as u64;
    
    group.bench_function("stack_channel_2P", |b| {
        b.iter(|| {
            let channel = Box::new(StackChannel::<u32, 65536, 2>::new());
            let channel_ptr = Box::into_raw(channel);
            let channel_ref: &'static StackChannel<u32, 65536, 2> = unsafe { &*channel_ptr };
            
            let handles: Vec<_> = (0..NUM_PRODUCERS)
                .map(|_| {
                    let producer = channel_ref.register().unwrap();
                    thread::spawn(move || {
                        let mut sent = 0u64;
                        while sent < msg_per_producer {
                            let want = MPSC_BATCH.min((msg_per_producer - sent) as usize);
                            unsafe {
                                if let Some((ptr, len)) = producer.reserve(want) {
                                    for i in 0..len {
                                        *ptr.add(i) = (sent + i as u64) as u32;
                                    }
                                    producer.commit(len);
                                    sent += len as u64;
                                } else {
                                    std::hint::spin_loop();
                                }
                            }
                        }
                    })
                })
                .collect();
            
            let mut total = 0u64;
            loop {
                let consumed = channel_ref.consume_all(|_| {});
                total += consumed as u64;
                if total >= MPSC_MSG_COUNT {
                    break;
                }
                if consumed == 0 {
                    std::hint::spin_loop();
                }
            }
            
            for h in handles {
                h.join().unwrap();
            }
            
            unsafe { drop(Box::from_raw(channel_ptr)); }
            black_box(total)
        });
    });
    
    group.finish();
}

/// Benchmark StackChannel with 4 producers
fn bench_stack_mpsc_4p(c: &mut Criterion) {
    let mut group = c.benchmark_group("mpsc_throughput");
    group.throughput(Throughput::Elements(MPSC_MSG_COUNT));
    
    const NUM_PRODUCERS: usize = 4;
    let msg_per_producer = MPSC_MSG_COUNT / NUM_PRODUCERS as u64;
    
    group.bench_function("stack_channel_4P", |b| {
        b.iter(|| {
            let channel = Box::new(StackChannel::<u32, 65536, 4>::new());
            let channel_ptr = Box::into_raw(channel);
            let channel_ref: &'static StackChannel<u32, 65536, 4> = unsafe { &*channel_ptr };
            
            let handles: Vec<_> = (0..NUM_PRODUCERS)
                .map(|_| {
                    let producer = channel_ref.register().unwrap();
                    thread::spawn(move || {
                        let mut sent = 0u64;
                        while sent < msg_per_producer {
                            let want = MPSC_BATCH.min((msg_per_producer - sent) as usize);
                            unsafe {
                                if let Some((ptr, len)) = producer.reserve(want) {
                                    for i in 0..len {
                                        *ptr.add(i) = (sent + i as u64) as u32;
                                    }
                                    producer.commit(len);
                                    sent += len as u64;
                                } else {
                                    std::hint::spin_loop();
                                }
                            }
                        }
                    })
                })
                .collect();
            
            let mut total = 0u64;
            loop {
                let consumed = channel_ref.consume_all(|_| {});
                total += consumed as u64;
                if total >= MPSC_MSG_COUNT {
                    break;
                }
                if consumed == 0 {
                    std::hint::spin_loop();
                }
            }
            
            for h in handles {
                h.join().unwrap();
            }
            
            unsafe { drop(Box::from_raw(channel_ptr)); }
            black_box(total)
        });
    });
    
    group.finish();
}

/// Benchmark StackChannel with 8 producers
fn bench_stack_mpsc_8p(c: &mut Criterion) {
    let mut group = c.benchmark_group("mpsc_throughput");
    group.throughput(Throughput::Elements(MPSC_MSG_COUNT));
    
    const NUM_PRODUCERS: usize = 8;
    let msg_per_producer = MPSC_MSG_COUNT / NUM_PRODUCERS as u64;
    
    group.bench_function("stack_channel_8P", |b| {
        b.iter(|| {
            let channel = Box::new(StackChannel::<u32, 65536, 8>::new());
            let channel_ptr = Box::into_raw(channel);
            let channel_ref: &'static StackChannel<u32, 65536, 8> = unsafe { &*channel_ptr };
            
            let handles: Vec<_> = (0..NUM_PRODUCERS)
                .map(|_| {
                    let producer = channel_ref.register().unwrap();
                    thread::spawn(move || {
                        let mut sent = 0u64;
                        while sent < msg_per_producer {
                            let want = MPSC_BATCH.min((msg_per_producer - sent) as usize);
                            unsafe {
                                if let Some((ptr, len)) = producer.reserve(want) {
                                    for i in 0..len {
                                        *ptr.add(i) = (sent + i as u64) as u32;
                                    }
                                    producer.commit(len);
                                    sent += len as u64;
                                } else {
                                    std::hint::spin_loop();
                                }
                            }
                        }
                    })
                })
                .collect();
            
            let mut total = 0u64;
            loop {
                let consumed = channel_ref.consume_all(|_| {});
                total += consumed as u64;
                if total >= MPSC_MSG_COUNT {
                    break;
                }
                if consumed == 0 {
                    std::hint::spin_loop();
                }
            }
            
            for h in handles {
                h.join().unwrap();
            }
            
            unsafe { drop(Box::from_raw(channel_ptr)); }
            black_box(total)
        });
    });
    
    group.finish();
}

criterion_group!(
    benches,
    bench_heap_single_thread,
    bench_stack_single_thread,
    bench_heap_spsc,
    bench_stack_spsc,
    bench_stack_sizes,
    bench_heap_mpsc,
    bench_stack_mpsc_2p,
    bench_stack_mpsc_4p,
    bench_stack_mpsc_8p,
);
criterion_main!(benches);
