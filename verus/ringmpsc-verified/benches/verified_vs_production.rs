//! Benchmarks: the verified ring against the production ring (phase 7, #26).
//!
//! ```text
//! CARGO_TARGET_DIR=target/plain cargo bench --bench verified_vs_production
//! ```
//!
//! Three things differ between the two rings, and these groups separate them:
//!
//! - **Atomic orderings.** The verified ring uses `vstd` atomics, which are
//!   sequentially consistent; production uses `Acquire`/`Release`. The
//!   `atomics` group measures that difference alone.
//! - **API shape.** The verified consumer returns owned items in a `Vec`
//!   (`release`, `consume_up_to`, `pop`); production hands them to a closure.
//!   The `single_thread` groups include that allocation.
//! - **Everything else** (`PCell` slots, `Arc` sharing, the erased ghost
//!   code, which costs nothing): visible in all groups.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use ringmpsc_rs::{Channel, Config, Ring};
use ringmpsc_verified::spsc::channel;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

/// 1024 slots.
const RING_BITS: u8 = 10;

fn quick(c: &mut Criterion) -> criterion::BenchmarkGroup<'_, criterion::measurement::WallTime> {
    let mut g = c.benchmark_group("ring");
    g.sample_size(20).measurement_time(Duration::from_secs(3)).warm_up_time(Duration::from_secs(1));
    g
}

/// One item in, one item out, repeatedly, on one thread.
fn single_thread_push_pop(c: &mut Criterion) {
    const N: u64 = 100_000;
    let mut g = quick(c);
    g.throughput(Throughput::Elements(N));

    g.bench_function(BenchmarkId::new("single_thread/push_pop", "production"), |b| {
        let mut ring = Ring::<u64>::new(Config::new(RING_BITS, 1, false));
        b.iter(|| {
            for i in 0..N {
                ring.push(i);
                ring.consume_up_to(1, |v| {
                    black_box(v);
                });
            }
        });
    });

    g.bench_function(BenchmarkId::new("single_thread/push_pop", "verified"), |b| {
        let (mut p, mut c) = channel::<u64>(RING_BITS);
        b.iter(|| {
            for i in 0..N {
                let _ = p.push(i);
                black_box(c.pop());
            }
        });
    });
    g.finish();
}

/// Reserve 64, write them, commit; then consume 64. One thread.
fn single_thread_batch(c: &mut Criterion) {
    const ROUNDS: u64 = 2_000;
    const BATCH: usize = 64;
    let mut g = quick(c);
    g.throughput(Throughput::Elements(ROUNDS * BATCH as u64));

    g.bench_function(BenchmarkId::new("single_thread/batch_64", "production"), |b| {
        let mut ring = Ring::<u64>::new(Config::new(RING_BITS, 1, false));
        b.iter(|| {
            for round in 0..ROUNDS {
                let base = round * BATCH as u64;
                let mut r = ring.reserve(BATCH).unwrap();
                r.extend(base..base + BATCH as u64);
                r.commit();
                ring.consume_up_to(BATCH, |v| {
                    black_box(v);
                });
            }
        });
    });

    g.bench_function(BenchmarkId::new("single_thread/batch_64", "verified"), |b| {
        let (mut p, mut c) = channel::<u64>(RING_BITS);
        b.iter(|| {
            for round in 0..ROUNDS {
                let base = round * BATCH as u64;
                assert_eq!(p.reserve(BATCH), BATCH);
                for v in base..base + BATCH as u64 {
                    p.write_next(v);
                }
                p.commit();
                black_box(c.consume_up_to(BATCH));
            }
        });
    });
    g.finish();
}

/// A producer thread and a consumer thread: 1M items in batches of 64.
/// Production runs through a one-producer `Channel` (a bare `Ring` can't be
/// split across threads); the verified ring through its two halves.
fn two_threads(c: &mut Criterion) {
    const N: u64 = 1_000_000;
    const BATCH: usize = 64;
    let mut g = quick(c);
    g.throughput(Throughput::Elements(N));

    g.bench_function(BenchmarkId::new("two_threads/1M_items", "production"), |b| {
        b.iter(|| {
            let channel = Channel::<u64>::new(Config::new(RING_BITS, 1, false));
            let mut producer = channel.register().unwrap();
            let mut consumer = channel.consumer().unwrap();
            let sender = thread::spawn(move || {
                let mut sent = 0u64;
                while sent < N {
                    let want = BATCH.min((N - sent) as usize);
                    match producer.reserve(want) {
                        Some(mut r) => {
                            let len = r.len() as u64;
                            r.extend(sent..sent + len);
                            sent += r.commit() as u64;
                        }
                        None => std::hint::spin_loop(),
                    }
                }
            });
            let mut got = 0u64;
            while got < N {
                let n = consumer.consume_all_up_to_owned(1024, |v| {
                    black_box(v);
                });
                got += n as u64;
                if n == 0 {
                    std::hint::spin_loop();
                }
            }
            sender.join().unwrap();
        });
    });

    g.bench_function(BenchmarkId::new("two_threads/1M_items", "verified"), |b| {
        b.iter(|| {
            let (mut p, mut c) = channel::<u64>(RING_BITS);
            let sender = thread::spawn(move || {
                let mut sent = 0u64;
                while sent < N {
                    let want = BATCH.min((N - sent) as usize);
                    if p.reserve(want) == want {
                        for v in sent..sent + want as u64 {
                            p.write_next(v);
                        }
                        sent += p.commit() as u64;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            });
            let mut got = 0u64;
            while got < N {
                let out = c.consume_up_to(1024);
                got += out.len() as u64;
                if out.is_empty() {
                    std::hint::spin_loop();
                }
                black_box(out);
            }
            sender.join().unwrap();
        });
    });
    g.finish();
}

/// The ordering cost alone: SeqCst against Release stores and Acquire loads
/// on one atomic, uncontended. On x86-64 a SeqCst store is an `xchg` and a
/// Release store a plain `mov`; on AArch64 both are `stlr`.
fn atomics(c: &mut Criterion) {
    const N: u64 = 1_000_000;
    let mut g = quick(c);
    g.throughput(Throughput::Elements(N));
    let a = AtomicU64::new(0);

    g.bench_function(BenchmarkId::new("atomics/store", "Release"), |b| {
        b.iter(|| {
            for i in 0..N {
                a.store(black_box(i), Ordering::Release);
            }
        });
    });
    g.bench_function(BenchmarkId::new("atomics/store", "SeqCst"), |b| {
        b.iter(|| {
            for i in 0..N {
                a.store(black_box(i), Ordering::SeqCst);
            }
        });
    });
    g.bench_function(BenchmarkId::new("atomics/load", "Acquire"), |b| {
        b.iter(|| {
            for _ in 0..N {
                black_box(a.load(Ordering::Acquire));
            }
        });
    });
    g.bench_function(BenchmarkId::new("atomics/load", "SeqCst"), |b| {
        b.iter(|| {
            for _ in 0..N {
                black_box(a.load(Ordering::SeqCst));
            }
        });
    });
    g.finish();
}

criterion_group!(benches, single_thread_push_pop, single_thread_batch, two_threads, atomics);
criterion_main!(benches);
