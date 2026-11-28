//! Multi-sender throughput of the stream wrapper.
//!
//! Each sender is a std thread calling `try_send` in a loop (yielding when its
//! ring is full); the receiver drains on a tokio runtime. Measures how the
//! per-send work in `RingSender` scales with the number of senders: shared
//! per-send state would stop it scaling.
//!
//! Run: `cargo bench -p ringmpsc-stream --bench stream_throughput`

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use ringmpsc_rs::{Backoff, Config};
use ringmpsc_stream::{channel, StreamExt};
use std::sync::Barrier;
use std::thread;
use std::time::{Duration, Instant};

const PER_SENDER: u64 = 200_000;

fn run(senders: usize) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    // The receiver's poll timer needs a runtime to be created in.
    let (factory, mut rx) = {
        let _ctx = runtime.enter();
        channel::<u64>(Config::new(12, senders, false))
    };
    let total = PER_SENDER * senders as u64;

    thread::scope(|s| {
        for _ in 0..senders {
            let mut tx = factory.register().expect("sender");
            s.spawn(move || {
                for i in 0..PER_SENDER {
                    let mut item = i;
                    // Ring full: back off (spin, then yield, then sleep),
                    // never a bare spin.
                    let mut backoff = Backoff::new();
                    while let Err(back) = tx.try_send(item) {
                        item = back;
                        if backoff.is_completed() {
                            thread::sleep(Duration::from_micros(10));
                        } else {
                            backoff.snooze();
                        }
                    }
                }
            });
        }
        runtime.block_on(async {
            let mut seen = 0;
            while seen < total {
                if rx.next().await.is_some() {
                    seen += 1;
                }
            }
        });
    });
}

/// Times only the senders: each ring holds every item (2^18 slots), so no
/// sender waits on the receiver, and the drain happens afterwards, untimed.
/// Shows the per-send cost in `RingSender` and whether it scales.
fn send_only(senders: usize, iters: u64) -> Duration {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    let mut elapsed = Duration::ZERO;
    for _ in 0..iters {
        let (factory, mut rx) = {
            let _ctx = runtime.enter();
            channel::<u64>(Config::new(18, senders, false))
        };
        let start = Barrier::new(senders + 1);
        let took = thread::scope(|s| {
            for _ in 0..senders {
                let mut tx = factory.register().expect("sender");
                let start = &start;
                s.spawn(move || {
                    start.wait();
                    for i in 0..PER_SENDER {
                        assert!(tx.try_send(i).is_ok(), "the ring holds every item");
                    }
                });
            }
            start.wait();
            let t0 = Instant::now();
            // Leaving the scope joins every sender.
            t0
        })
        .elapsed();
        elapsed += took;
        factory.close();
        runtime.block_on(async { while rx.next().await.is_some() {} });
    }
    elapsed
}

fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_send_only");
    group.sample_size(20);
    for senders in [1usize, 4] {
        group.throughput(Throughput::Elements(PER_SENDER * senders as u64));
        group.bench_with_input(BenchmarkId::from_parameter(senders), &senders, |b, &n| {
            b.iter_custom(|iters| send_only(n, iters));
        });
    }
    group.finish();

    let mut group = c.benchmark_group("stream_try_send");
    group.sample_size(20);
    for senders in [1usize, 4] {
        group.throughput(Throughput::Elements(PER_SENDER * senders as u64));
        group.bench_with_input(BenchmarkId::from_parameter(senders), &senders, |b, &n| {
            b.iter(|| run(n));
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
