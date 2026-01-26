//! Integration tests for StackRing
//!
//! These tests verify the StackRing implementation under realistic conditions
//! including multi-threaded access and stress testing.

#![cfg(feature = "stack-ring")]

use ringmpsc_rs::StackRing;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

const MSG_COUNT: u64 = 100_000;

/// Test basic SPSC pattern with separate producer and consumer threads.
#[test]
fn test_stack_ring_spsc_threads() {
    // Use Box::leak to get 'static lifetime for thread sharing
    let ring: &'static StackRing<u64, 4096> = Box::leak(Box::new(StackRing::new()));
    let received = Arc::new(AtomicU64::new(0));
    let received_clone = Arc::clone(&received);

    // Producer thread
    let producer = thread::spawn(move || {
        let mut sent = 0u64;
        while sent < MSG_COUNT {
            unsafe {
                if let Some((ptr, len)) = ring.reserve(1) {
                    *ptr = sent;
                    ring.commit(len);
                    sent += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
        }
        ring.close();
    });

    // Consumer thread
    let consumer = thread::spawn(move || {
        let mut sum = 0u64;
        let mut count = 0u64;
        loop {
            unsafe {
                let consumed = ring.consume_batch(|v| {
                    sum += v;
                });
                count += consumed as u64;
                if consumed == 0 {
                    if ring.is_closed() && ring.is_empty() {
                        break;
                    }
                    std::hint::spin_loop();
                }
            }
        }
        received_clone.store(count, Ordering::Release);
        sum
    });

    producer.join().unwrap();
    let sum = consumer.join().unwrap();

    assert_eq!(received.load(Ordering::Acquire), MSG_COUNT);
    // Sum of 0..MSG_COUNT = MSG_COUNT * (MSG_COUNT - 1) / 2
    let expected_sum = MSG_COUNT * (MSG_COUNT - 1) / 2;
    assert_eq!(sum, expected_sum);
}

/// Test FIFO ordering is preserved.
#[test]
fn test_stack_ring_fifo_order() {
    let ring: &'static StackRing<u64, 1024> = Box::leak(Box::new(StackRing::new()));

    let producer = thread::spawn(move || {
        for i in 0..10_000u64 {
            loop {
                unsafe {
                    if let Some((ptr, len)) = ring.reserve(1) {
                        *ptr = i;
                        ring.commit(len);
                        break;
                    }
                }
                std::hint::spin_loop();
            }
        }
        ring.close();
    });

    let consumer = thread::spawn(move || {
        let mut expected = 0u64;
        let mut violations = 0u64;

        loop {
            unsafe {
                let consumed = ring.consume_batch(|v| {
                    if *v != expected {
                        violations += 1;
                    }
                    expected += 1;
                });
                if consumed == 0 && ring.is_closed() && ring.is_empty() {
                    break;
                }
            }
        }
        violations
    });

    producer.join().unwrap();
    let violations = consumer.join().unwrap();
    assert_eq!(violations, 0, "FIFO order violated");
}

/// Test batch reservations and wrap-around handling.
#[test]
fn test_stack_ring_batch_wrap() {
    let ring: StackRing<u64, 64> = StackRing::new();
    let mut total_sent = 0u64;
    let mut total_received = 0u64;

    // Multiple rounds of write-read to exercise wrap-around
    for round in 0..100 {
        // Write a batch
        let batch_size = (round % 30) + 1;
        let mut sent_this_batch = 0;

        while sent_this_batch < batch_size {
            unsafe {
                if let Some((ptr, len)) = ring.reserve(batch_size - sent_this_batch) {
                    for i in 0..len {
                        *ptr.add(i) = total_sent + i as u64;
                    }
                    ring.commit(len);
                    sent_this_batch += len;
                    total_sent += len as u64;
                }
            }
        }

        // Read all
        unsafe {
            ring.consume_batch(|_| {
                total_received += 1;
            });
        }
    }

    assert_eq!(total_sent, total_received);
}

/// Stress test with rapid producer/consumer cycling.
#[test]
fn test_stack_ring_stress() {
    let ring: &'static StackRing<u64, 8192> = Box::leak(Box::new(StackRing::new()));
    let messages = 500_000u64;

    let producer = thread::spawn(move || {
        let mut sent = 0u64;
        while sent < messages {
            let batch = ((messages - sent) as usize).min(64);
            unsafe {
                if let Some((ptr, len)) = ring.reserve(batch) {
                    for i in 0..len {
                        *ptr.add(i) = sent + i as u64;
                    }
                    ring.commit(len);
                    sent += len as u64;
                } else {
                    std::hint::spin_loop();
                }
            }
        }
        ring.close();
        sent
    });

    let consumer = thread::spawn(move || {
        let mut received = 0u64;
        loop {
            unsafe {
                let count = ring.consume_batch(|_| {});
                received += count as u64;
                if count == 0 && ring.is_closed() && ring.is_empty() {
                    break;
                }
            }
        }
        received
    });

    let sent = producer.join().unwrap();
    let received = consumer.join().unwrap();

    assert_eq!(sent, messages);
    assert_eq!(received, messages);
}

/// Test with non-Copy types to verify Drop behavior.
#[test]
fn test_stack_ring_string_type() {
    use std::sync::atomic::AtomicUsize;

    static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

    struct TrackedString(#[allow(dead_code)] String);

    impl Drop for TrackedString {
        fn drop(&mut self) {
            DROP_COUNT.fetch_add(1, Ordering::SeqCst);
        }
    }

    DROP_COUNT.store(0, Ordering::SeqCst);

    {
        let ring: StackRing<TrackedString, 64> = StackRing::new();

        // Write 20 items
        for i in 0..20 {
            unsafe {
                let (ptr, _) = ring.reserve(1).unwrap();
                std::ptr::write(ptr, TrackedString(format!("item_{}", i)));
                ring.commit(1);
            }
        }

        // Consume 10
        unsafe {
            ring.consume_up_to(10, |_| {});
        }
        assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 10);

        // Ring drops with 10 remaining
    }

    assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 20);
}
