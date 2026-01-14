use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use ringmpsc_rs::{Channel, Config};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

const MSG_PER_PRODUCER: u64 = 10_000_000; // 10M messages per producer
const BATCH_SIZE: usize = 4096; // Batch size for zero-copy operations

fn bench_spsc(c: &mut Criterion) {
    let mut group = c.benchmark_group("spsc");
    group.throughput(Throughput::Elements(MSG_PER_PRODUCER));

    group.bench_function("single_producer_consumer", |b| {
        b.iter(|| {
            let config = Config::default();
            let channel = Arc::new(Channel::<u32>::new(config));
            let producer = channel.register().unwrap();

            // Producer thread
            let ch = Arc::clone(&channel);
            let producer_handle = thread::spawn(move || {
                let mut sent = 0u64;
                while sent < MSG_PER_PRODUCER {
                    let want = BATCH_SIZE.min((MSG_PER_PRODUCER - sent) as usize);
                    if let Some(mut r) = producer.reserve(want) {
                        let len = {
                            let slice = r.as_mut_slice();
                            for (i, item) in slice.iter_mut().enumerate() {
                                item.write((sent + i as u64) as u32);
                            }
                            slice.len()
                        };
                        r.commit();
                        sent += len as u64;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            });

            // Consumer thread
            let mut count = 0u64;
            while count < MSG_PER_PRODUCER {
                count += ch.consume_all(|item| {
                    black_box(item);
                }) as u64;
                if count < MSG_PER_PRODUCER {
                    std::hint::spin_loop();
                }
            }

            producer_handle.join().unwrap();
        });
    });

    group.finish();
}

fn bench_mpsc(c: &mut Criterion) {
    let mut group = c.benchmark_group("mpsc");

    for num_producers in [2, 4, 8].iter() {
        let total_msgs = MSG_PER_PRODUCER * (*num_producers as u64);
        group.throughput(Throughput::Elements(total_msgs));

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}P_{}C", num_producers, num_producers)),
            num_producers,
            |b, &n| {
                b.iter(|| {
                    let config = Config::new(16, n.max(16), false);
                    let channel = Arc::new(Channel::<u32>::new(config));
                    
                    let mut producer_handles = vec![];
                    
                    // Spawn producer threads
                    for _ in 0..n {
                        let ch = Arc::clone(&channel);
                        let handle = thread::spawn(move || {
                            let producer = ch.register().unwrap();
                            let mut sent = 0u64;
                            
                            while sent < MSG_PER_PRODUCER {
                                let want = BATCH_SIZE.min((MSG_PER_PRODUCER - sent) as usize);
                                if let Some(mut r) = producer.reserve(want) {
                                    let len = {
                                        let slice = r.as_mut_slice();
                                        for (i, item) in slice.iter_mut().enumerate() {
                                            item.write((sent + i as u64) as u32);
                                        }
                                        slice.len()
                                    };
                                    r.commit();
                                    sent += len as u64;
                                } else {
                                    std::hint::spin_loop();
                                }
                            }
                        });
                        producer_handles.push(handle);
                    }

                    // Consumer thread
                    let ch = Arc::clone(&channel);
                    let consumer_handle = thread::spawn(move || {
                        let mut count = 0u64;
                        let target = MSG_PER_PRODUCER * (n as u64);
                        
                        while count < target {
                            count += ch.consume_all(|item| {
                                black_box(item);
                            }) as u64;
                            if count < target {
                                std::hint::spin_loop();
                            }
                        }
                        count
                    });

                    // Wait for all producers
                    for handle in producer_handles {
                        handle.join().unwrap();
                    }

                    // Wait for consumer
                    let count = consumer_handle.join().unwrap();
                    assert_eq!(count, MSG_PER_PRODUCER * (n as u64));
                });
            },
        );
    }

    group.finish();
}

fn bench_batch_sizes(c: &mut Criterion) {
    let mut group = c.benchmark_group("batch_sizes");
    group.throughput(Throughput::Elements(MSG_PER_PRODUCER));

    for batch_size in [256, 1024, 4096, 16384].iter() {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("batch_{}", batch_size)),
            batch_size,
            |b, &batch| {
                b.iter(|| {
                    let config = Config::default();
                    let channel = Arc::new(Channel::<u32>::new(config));
                    let producer = channel.register().unwrap();

                    let ch = Arc::clone(&channel);
                    let producer_handle = thread::spawn(move || {
                        let mut sent = 0u64;
                        while sent < MSG_PER_PRODUCER {
                            let want = batch.min((MSG_PER_PRODUCER - sent) as usize);
                            if let Some(mut r) = producer.reserve(want) {
                                let len = {
                                    let slice = r.as_mut_slice();
                                    for (i, item) in slice.iter_mut().enumerate() {
                                        item.write((sent + i as u64) as u32);
                                    }
                                    slice.len()
                                };
                                r.commit();
                                sent += len as u64;
                            } else {
                                std::hint::spin_loop();
                            }
                        }
                    });

                    let mut count = 0u64;
                    while count < MSG_PER_PRODUCER {
                        count += ch.consume_all_up_to(batch, |item| {
                            black_box(item);
                        }) as u64;
                        if count < MSG_PER_PRODUCER {
                            std::hint::spin_loop();
                        }
                    }

                    producer_handle.join().unwrap();
                });
            },
        );
    }

    group.finish();
}

fn bench_zero_copy(c: &mut Criterion) {
    let mut group = c.benchmark_group("zero_copy");
    
    let msgs = 1_000_000u64;
    group.throughput(Throughput::Elements(msgs));

    // Zero-copy reserve/commit pattern
    group.bench_function("reserve_commit", |b| {
        b.iter(|| {
            let config = Config::default();
            let channel = Arc::new(Channel::<[u64; 8]>::new(config));
            let producer = channel.register().unwrap();

            let ch = Arc::clone(&channel);
            let producer_handle = thread::spawn(move || {
                let mut sent = 0u64;
                while sent < msgs {
                    let want = 1024.min((msgs - sent) as usize);
                    if let Some(mut r) = producer.reserve(want) {
                        let len = {
                            let slice = r.as_mut_slice();
                            for (i, item) in slice.iter_mut().enumerate() {
                                item.write([(sent + i as u64); 8]);
                            }
                            slice.len()
                        };
                        r.commit();
                        sent += len as u64;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            });

            let mut count = 0u64;
            while count < msgs {
                count += ch.consume_all(|item| {
                    black_box(item);
                }) as u64;
                if count < msgs {
                    std::hint::spin_loop();
                }
            }

            producer_handle.join().unwrap();
        });
    });

    group.finish();
}

fn bench_contention(c: &mut Criterion) {
    let mut group = c.benchmark_group("contention");
    
    // Test with high contention: many producers, small ring
    let config = Config::new(12, 16, false); // 4K slots
    let msgs = 100_000u64;
    
    for num_producers in [4, 8].iter() {
        let total = msgs * (*num_producers as u64);
        group.throughput(Throughput::Elements(total));
        
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}P_small_ring", num_producers)),
            num_producers,
            |b, &n| {
                b.iter(|| {
                    let channel = Arc::new(Channel::<u32>::new(config));
                    let counter = Arc::new(AtomicU64::new(0));
                    
                    let mut handles = vec![];
                    
                    // Producers
                    for _ in 0..n {
                        let ch = Arc::clone(&channel);
                        let handle = thread::spawn(move || {
                            let producer = ch.register().unwrap();
                            let mut sent = 0u64;
                            
                            while sent < msgs {
                                if let Some(mut r) = producer.reserve(1) {
                                    r.as_mut_slice()[0].write(sent as u32);
                                    r.commit();
                                    sent += 1;
                                } else {
                                    std::hint::spin_loop();
                                }
                            }
                        });
                        handles.push(handle);
                    }
                    
                    // Consumer
                    let ch = Arc::clone(&channel);
                    let cnt = Arc::clone(&counter);
                    let consumer = thread::spawn(move || {
                        let target = msgs * (n as u64);
                        while cnt.load(Ordering::Relaxed) < target {
                            let consumed = ch.consume_all(|item| {
                                black_box(item);
                            });
                            cnt.fetch_add(consumed as u64, Ordering::Relaxed);
                            if consumed == 0 {
                                std::hint::spin_loop();
                            }
                        }
                    });
                    
                    for h in handles {
                        h.join().unwrap();
                    }
                    consumer.join().unwrap();
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_spsc,
    bench_mpsc,
    bench_batch_sizes,
    bench_zero_copy,
    bench_contention
);
criterion_main!(benches);
