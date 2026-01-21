//! Integration tests for StackChannel - Multi-producer MPSC on stack.

#![cfg(feature = "stack-ring")]

use ringmpsc_rs::{StackChannel, StackChannelError};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

/// Test that each producer maintains FIFO ordering in its own stream.
#[test]
fn test_stack_channel_per_producer_fifo() {
    const NUM_PRODUCERS: usize = 4;
    const ITEMS_PER_PRODUCER: usize = 1000;

    // Use Box::leak to get 'static lifetime for thread sharing
    let channel: &'static StackChannel<(usize, usize), 4096, 4> =
        Box::leak(Box::new(StackChannel::new()));

    let handles: Vec<_> = (0..NUM_PRODUCERS)
        .map(|producer_id| {
            let producer = channel.register().unwrap();
            thread::spawn(move || {
                for seq in 0..ITEMS_PER_PRODUCER {
                    // Retry with backpressure handling
                    while !producer.push((producer_id, seq)) {
                        std::hint::spin_loop();
                    }
                }
            })
        })
        .collect();

    // Track last seen sequence per producer
    let mut last_seq: HashMap<usize, usize> = HashMap::new();
    let mut total_received = 0;

    // Consume until we have all items
    while total_received < NUM_PRODUCERS * ITEMS_PER_PRODUCER {
        let consumed = channel.consume_all(|(producer_id, seq)| {
            // Verify FIFO order per producer
            if let Some(&prev) = last_seq.get(producer_id) {
                assert_eq!(
                    *seq,
                    prev + 1,
                    "Producer {} FIFO violation: expected {} but got {}",
                    producer_id,
                    prev + 1,
                    seq
                );
            } else {
                assert_eq!(*seq, 0, "Producer {} should start at 0", producer_id);
            }
            last_seq.insert(*producer_id, *seq);
        });
        total_received += consumed;

        if consumed == 0 {
            std::hint::spin_loop();
        }
    }

    for h in handles {
        h.join().unwrap();
    }

    // Verify all producers sent complete sequences
    for producer_id in 0..NUM_PRODUCERS {
        assert_eq!(
            last_seq.get(&producer_id),
            Some(&(ITEMS_PER_PRODUCER - 1)),
            "Producer {} incomplete",
            producer_id
        );
    }

    // Cleanup
    unsafe {
        drop(Box::from_raw(
            channel as *const _ as *mut StackChannel<(usize, usize), 4096, 4>,
        ));
    }
}

/// Stress test with concurrent producers and consumer.
#[test]
fn test_stack_channel_stress() {
    const NUM_PRODUCERS: usize = 8;
    const ITEMS_PER_PRODUCER: usize = 50_000;
    const TOTAL_ITEMS: usize = NUM_PRODUCERS * ITEMS_PER_PRODUCER;

    let channel: &'static StackChannel<u64, 8192, 8> =
        Box::leak(Box::new(StackChannel::new()));

    let sent_count = Arc::new(AtomicUsize::new(0));

    // Spawn producers
    let handles: Vec<_> = (0..NUM_PRODUCERS)
        .map(|i| {
            let producer = channel.register().unwrap();
            let sent_count = Arc::clone(&sent_count);
            thread::spawn(move || {
                for j in 0..ITEMS_PER_PRODUCER {
                    let value = (i * ITEMS_PER_PRODUCER + j) as u64;
                    while !producer.push(value) {
                        std::hint::spin_loop();
                    }
                    sent_count.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();

    // Consumer
    let mut received_sum = 0u64;
    let mut received_count = 0usize;

    while received_count < TOTAL_ITEMS {
        let consumed = channel.consume_all(|v| {
            received_sum += v;
        });
        received_count += consumed;

        if consumed == 0 {
            std::hint::spin_loop();
        }
    }

    for h in handles {
        h.join().unwrap();
    }

    // Verify: sum of 0..TOTAL_ITEMS = TOTAL_ITEMS * (TOTAL_ITEMS - 1) / 2
    let expected_sum: u64 = (0..TOTAL_ITEMS as u64).sum();
    assert_eq!(received_sum, expected_sum, "Sum mismatch - data loss or corruption");
    assert_eq!(received_count, TOTAL_ITEMS);

    // Cleanup
    unsafe {
        drop(Box::from_raw(
            channel as *const _ as *mut StackChannel<u64, 8192, 8>,
        ));
    }
}

/// Test consume_all_owned with String (verifies ownership transfer).
#[test]
fn test_stack_channel_owned_strings() {
    let channel: &'static StackChannel<String, 256, 4> =
        Box::leak(Box::new(StackChannel::new()));

    let handles: Vec<_> = (0..4)
        .map(|i| {
            let producer = channel.register().unwrap();
            thread::spawn(move || {
                for j in 0..100 {
                    let msg = format!("producer_{}_msg_{}", i, j);
                    while !producer.push(msg.clone()) {
                        std::hint::spin_loop();
                    }
                }
            })
        })
        .collect();

    let mut owned_messages: Vec<String> = Vec::new();
    let mut count = 0;

    while count < 400 {
        let consumed = channel.consume_all_owned(|s| {
            owned_messages.push(s);
        });
        count += consumed;

        if consumed == 0 {
            std::hint::spin_loop();
        }
    }

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(owned_messages.len(), 400);

    // Verify format
    for msg in &owned_messages {
        assert!(msg.starts_with("producer_"));
        assert!(msg.contains("_msg_"));
    }

    // Cleanup
    unsafe {
        drop(Box::from_raw(
            channel as *const _ as *mut StackChannel<String, 256, 4>,
        ));
    }
}

/// Test producer registration limits.
#[test]
fn test_stack_channel_registration_limit() {
    let channel: StackChannel<u64, 16, 3> = StackChannel::new();

    let p1 = channel.register();
    let p2 = channel.register();
    let p3 = channel.register();
    let p4 = channel.register();

    assert!(p1.is_ok());
    assert!(p2.is_ok());
    assert!(p3.is_ok());
    assert!(matches!(
        p4,
        Err(StackChannelError::TooManyProducers { max: 3 })
    ));
}

/// Test channel close behavior.
#[test]
fn test_stack_channel_close() {
    let channel: StackChannel<u64, 64, 4> = StackChannel::new();

    let producer = channel.register().unwrap();
    producer.push(100);
    producer.push(200);

    channel.close();

    // New registration should fail
    assert!(matches!(
        channel.register(),
        Err(StackChannelError::Closed)
    ));

    // Existing producer can't push new items
    assert!(!producer.push(300));

    // But we can still drain existing items
    let mut values = Vec::new();
    channel.consume_all(|v| values.push(*v));
    assert_eq!(values, vec![100, 200]);
}

/// Test batch reservation and commit.
#[test]
fn test_stack_channel_batch_reserve_commit() {
    let channel: StackChannel<u64, 128, 2> = StackChannel::new();
    let producer = channel.register().unwrap();

    // Reserve and write a batch
    unsafe {
        if let Some((ptr, len)) = producer.reserve(50) {
            assert!(len > 0 && len <= 50);
            for i in 0..len {
                *ptr.add(i) = (i * 10) as u64;
            }
            producer.commit(len);
        }
    }

    let mut values = Vec::new();
    channel.consume_all(|v| values.push(*v));

    // Verify sequential values
    for (i, v) in values.iter().enumerate() {
        assert_eq!(*v, (i * 10) as u64);
    }
}

/// Test that consume_all_up_to respects the limit.
#[test]
fn test_stack_channel_consume_up_to_limit() {
    let channel: StackChannel<u64, 128, 2> = StackChannel::new();

    let p1 = channel.register().unwrap();
    let p2 = channel.register().unwrap();

    // p1 sends 10 items, p2 sends 10 items
    for i in 0..10 {
        p1.push(i);
        p2.push(100 + i);
    }

    // Consume at most 5
    let mut batch1 = Vec::new();
    let consumed1 = channel.consume_all_up_to(5, |v| batch1.push(*v));
    assert_eq!(consumed1, 5);
    assert_eq!(batch1.len(), 5);

    // Consume another 10
    let mut batch2 = Vec::new();
    let consumed2 = channel.consume_all_up_to(10, |v| batch2.push(*v));
    assert_eq!(consumed2, 10);

    // Consume rest
    let mut batch3 = Vec::new();
    let consumed3 = channel.consume_all_up_to(100, |v| batch3.push(*v));
    assert_eq!(consumed3, 5); // 20 total - 5 - 10 = 5 remaining
}
