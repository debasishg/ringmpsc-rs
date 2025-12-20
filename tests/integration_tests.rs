use ringmpsc_rs::{Channel, Config};
use std::sync::Arc;
use std::thread;

#[test]
fn test_fifo_ordering_single_producer() {
    let channel = Channel::<u64>::new(Config::default());
    let producer = channel.register().unwrap();

    const N: u64 = 10_000;

    // Send N items
    for i in 0..N {
        if let Some(mut r) = producer.reserve(1) {
            r.as_mut_slice()[0] = i;
            r.commit();
        }
    }

    // Verify FIFO order
    let mut expected = 0;
    let consumed = channel.consume_all(|item| {
        assert_eq!(*item, expected, "FIFO violation: expected {}, got {}", expected, item);
        expected += 1;
    });

    assert_eq!(consumed, N as usize);
    assert_eq!(expected, N);
}

#[test]
fn test_fifo_ordering_multi_producer() {
    const N_PRODUCERS: usize = 4;
    const ITEMS_PER_PRODUCER: u64 = 5_000;

    let channel = Arc::new(Channel::<(usize, u64)>::new(Config::default()));
    let mut handles = vec![];

    // Spawn producers
    for producer_id in 0..N_PRODUCERS {
        let ch = Arc::clone(&channel);
        let handle = thread::spawn(move || {
            let producer = ch.register().unwrap();
            for i in 0..ITEMS_PER_PRODUCER {
                if let Some(mut r) = producer.reserve(1) {
                    r.as_mut_slice()[0] = (producer_id, i);
                    r.commit();
                }
            }
        });
        handles.push(handle);
    }

    // Wait for all producers
    for handle in handles {
        handle.join().unwrap();
    }

    // Verify per-producer FIFO
    let mut last_seen = vec![0u64; N_PRODUCERS];
    let consumed = channel.consume_all(|(producer_id, value)| {
        assert_eq!(
            *value, last_seen[*producer_id],
            "FIFO violation for producer {}: expected {}, got {}",
            producer_id, last_seen[*producer_id], value
        );
        last_seen[*producer_id] += 1;
    });

    assert_eq!(consumed, N_PRODUCERS * ITEMS_PER_PRODUCER as usize);
    for (id, &count) in last_seen.iter().enumerate() {
        assert_eq!(
            count, ITEMS_PER_PRODUCER,
            "Producer {} sent {} items instead of {}",
            id, count, ITEMS_PER_PRODUCER
        );
    }
}

#[test]
fn test_concurrent_stress() {
    const N_PRODUCERS: usize = 8;
    const ITEMS_PER_PRODUCER: u64 = 50_000;

    let channel = Arc::new(Channel::<u64>::new(Config::default()));
    let mut handles = vec![];

    // Spawn producers
    for _ in 0..N_PRODUCERS {
        let ch = Arc::clone(&channel);
        let handle = thread::spawn(move || {
            let producer = ch.register().unwrap();
            for i in 0..ITEMS_PER_PRODUCER {
                while producer.reserve(1).is_none() {
                    thread::yield_now();
                }
                if let Some(mut r) = producer.reserve(1) {
                    r.as_mut_slice()[0] = i;
                    r.commit();
                }
            }
        });
        handles.push(handle);
    }

    // Consumer thread
    let ch = Arc::clone(&channel);
    let consumer_handle = thread::spawn(move || {
        let mut total = 0;
        let mut sum = 0u64;
        while total < N_PRODUCERS * ITEMS_PER_PRODUCER as usize {
            total += ch.consume_all(|item| sum += item);
            if total < N_PRODUCERS * ITEMS_PER_PRODUCER as usize {
                thread::yield_now();
            }
        }
        (total, sum)
    });

    // Wait for all producers
    for handle in handles {
        handle.join().unwrap();
    }

    // Wait for consumer
    let (total, sum) = consumer_handle.join().unwrap();

    let expected_sum = (0..ITEMS_PER_PRODUCER).sum::<u64>() * N_PRODUCERS as u64;
    assert_eq!(total, N_PRODUCERS * ITEMS_PER_PRODUCER as usize);
    assert_eq!(sum, expected_sum);
}

#[test]
fn test_batch_operations() {
    let channel = Channel::<u64>::new(Config::default());
    let producer = channel.register().unwrap();

    // Send in batches
    const BATCH_SIZE: usize = 100;
    const N_BATCHES: usize = 100;

    for batch in 0..N_BATCHES {
        if let Some(mut r) = producer.reserve(BATCH_SIZE) {
            let slice = r.as_mut_slice();
            for i in 0..slice.len() {
                slice[i] = (batch * BATCH_SIZE + i) as u64;
            }
            r.commit();
        }
    }

    // Consume in batches
    let mut received = vec![];
    let consumed = channel.consume_all(|item| {
        received.push(*item);
    });

    assert_eq!(consumed, BATCH_SIZE * N_BATCHES);
    assert_eq!(received.len(), BATCH_SIZE * N_BATCHES);

    // Verify order
    for (i, &val) in received.iter().enumerate() {
        assert_eq!(val, i as u64);
    }
}

#[test]
fn test_wrap_around() {
    // Small ring to force wrap-around
    let config = Config::new(8, 16, false); // 256 slots
    let channel = Channel::<u64>::new(config);
    let producer = channel.register().unwrap();

    const N: usize = 10_000; // Much larger than capacity

    // Interleaved send/receive to force wrapping
    for i in 0..N {
        if let Some(mut r) = producer.reserve(1) {
            r.as_mut_slice()[0] = i as u64;
            r.commit();
        }

        if i % 10 == 0 {
            channel.consume_all(|_| {});
        }
    }

    // Consume remaining
    let mut received = 0;
    channel.consume_all(|_| received += 1);

    assert!(received > 0);
}

#[test]
fn test_consume_up_to_limit() {
    let channel = Channel::<u64>::new(Config::default());
    let producer = channel.register().unwrap();

    // Send 1000 items
    for i in 0..1000 {
        if let Some(mut r) = producer.reserve(1) {
            r.as_mut_slice()[0] = i;
            r.commit();
        }
    }

    // Consume in chunks of 100
    let mut total = 0;
    for _ in 0..10 {
        let consumed = channel.consume_all_up_to(100, |_| {});
        assert!(consumed <= 100);
        total += consumed;
    }

    assert_eq!(total, 1000);
}
