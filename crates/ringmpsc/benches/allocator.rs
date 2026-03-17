//! Benchmarks comparing allocator strategies.
//!
//! Measures SPSC throughput for:
//! - `HeapAllocator` (default, Box<[`MaybeUninit`<T>]>)
//! - `AlignedAllocator<128>` (128-byte cache-line alignment)
//! - `AlignedAllocator<{2 MiB}>` (huge-page alignment)
//!
//! Run with: `cargo bench -p ringmpsc-rs --bench allocator`

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use ringmpsc_rs::{AlignedAllocator, Channel, Config, Ring};
use std::sync::Arc;
use std::thread;

const MSG_COUNT: u64 = 5_000_000;
const BATCH_SIZE: usize = 1024;

// =============================================================================
// Single-threaded: reserve/commit/consume cycle (no threading overhead)
// =============================================================================

fn bench_single_thread(c: &mut Criterion) {
    let mut group = c.benchmark_group("single_thread");
    group.throughput(Throughput::Elements(MSG_COUNT));

    // HeapAllocator (default)
    group.bench_function("heap", |b| {
        let ring = Ring::<u32>::new(Config::default());

        b.iter(|| {
            let mut sent = 0u64;
            let mut received = 0u64;

            while received < MSG_COUNT {
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

                received += ring.consume_batch(|item| {
                    black_box(item);
                }) as u64;
            }
        });
    });

    // AlignedAllocator<128> (cache-line aligned)
    group.bench_function("aligned_128", |b| {
        let ring =
            Ring::<u32, AlignedAllocator<128>>::new_in(Config::default(), AlignedAllocator::<128>);

        b.iter(|| {
            let mut sent = 0u64;
            let mut received = 0u64;

            while received < MSG_COUNT {
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

                received += ring.consume_batch(|item| {
                    black_box(item);
                }) as u64;
            }
        });
    });

    group.finish();
}

// =============================================================================
// Multi-threaded SPSC: producer thread + consumer thread
// =============================================================================

fn bench_spsc_threaded(c: &mut Criterion) {
    let mut group = c.benchmark_group("spsc_threaded");
    group.throughput(Throughput::Elements(MSG_COUNT));

    // HeapAllocator
    group.bench_function("heap", |b| {
        b.iter(|| {
            let channel = Arc::new(Channel::<u32>::new(Config::default()));
            let producer = channel.register().unwrap();

            let ch = Arc::clone(&channel);
            let producer_handle = thread::spawn(move || {
                let mut sent = 0u64;
                while sent < MSG_COUNT {
                    let want = BATCH_SIZE.min((MSG_COUNT - sent) as usize);
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
            });

            let mut count = 0u64;
            while count < MSG_COUNT {
                count += ch.consume_all(|item| {
                    black_box(item);
                }) as u64;
                if count < MSG_COUNT {
                    std::hint::spin_loop();
                }
            }

            producer_handle.join().unwrap();
        });
    });

    // AlignedAllocator<128>
    group.bench_function("aligned_128", |b| {
        b.iter(|| {
            let channel = Arc::new(Channel::<u32, AlignedAllocator<128>>::new_in(
                Config::default(),
                AlignedAllocator::<128>,
            ));
            let producer = channel.register().unwrap();

            let ch = Arc::clone(&channel);
            let producer_handle = thread::spawn(move || {
                let mut sent = 0u64;
                while sent < MSG_COUNT {
                    let want = BATCH_SIZE.min((MSG_COUNT - sent) as usize);
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
            });

            let mut count = 0u64;
            while count < MSG_COUNT {
                count += ch.consume_all(|item| {
                    black_box(item);
                }) as u64;
                if count < MSG_COUNT {
                    std::hint::spin_loop();
                }
            }

            producer_handle.join().unwrap();
        });
    });

    group.finish();
}

// =============================================================================
// MPSC: 4 producers, comparing allocators
// =============================================================================

fn bench_mpsc(c: &mut Criterion) {
    let mut group = c.benchmark_group("mpsc_allocator");
    let per_producer = 1_000_000u64;
    let num_producers = 4;
    let total = per_producer * num_producers;
    group.throughput(Throughput::Elements(total));

    // HeapAllocator
    group.bench_function("heap_4P", |b| {
        b.iter(|| {
            let config = Config::new(16, 16, false);
            let channel = Arc::new(Channel::<u32>::new(config));

            let mut handles = vec![];
            for _ in 0..num_producers {
                let ch = Arc::clone(&channel);
                handles.push(thread::spawn(move || {
                    let producer = ch.register().unwrap();
                    let mut sent = 0u64;
                    while sent < per_producer {
                        let want = BATCH_SIZE.min((per_producer - sent) as usize);
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
                }));
            }

            let mut count = 0u64;
            while count < total {
                count += channel.consume_all(|item| {
                    black_box(item);
                }) as u64;
                if count < total {
                    std::hint::spin_loop();
                }
            }

            for h in handles {
                h.join().unwrap();
            }
        });
    });

    // AlignedAllocator<128>
    group.bench_function("aligned_128_4P", |b| {
        b.iter(|| {
            let config = Config::new(16, 16, false);
            let channel = Arc::new(Channel::<u32, AlignedAllocator<128>>::new_in(
                config,
                AlignedAllocator::<128>,
            ));

            let mut handles = vec![];
            for _ in 0..num_producers {
                let ch = Arc::clone(&channel);
                handles.push(thread::spawn(move || {
                    let producer = ch.register().unwrap();
                    let mut sent = 0u64;
                    while sent < per_producer {
                        let want = BATCH_SIZE.min((per_producer - sent) as usize);
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
                }));
            }

            let mut count = 0u64;
            while count < total {
                count += channel.consume_all(|item| {
                    black_box(item);
                }) as u64;
                if count < total {
                    std::hint::spin_loop();
                }
            }

            for h in handles {
                h.join().unwrap();
            }
        });
    });

    group.finish();
}

criterion_group!(benches, bench_single_thread, bench_spsc_threaded, bench_mpsc);
criterion_main!(benches);
