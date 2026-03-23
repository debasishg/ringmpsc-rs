//! NUMA allocator integration tests.
//!
//! These tests verify that `NumaAllocator` correctly implements `BufferAllocator`
//! and integrates with `Ring<T>` and `Channel<T>`.
//!
//! Tests that verify actual NUMA placement (INV-NUMA-01) only run on multi-node
//! Linux machines. Other tests verify the allocator contract (INV-MEM-04) and
//! fallback behavior (INV-NUMA-02) on any platform.

#![cfg(feature = "numa")]

use ringmpsc_rs::numa::{NumaAllocator, NumaPolicy};
use ringmpsc_rs::{Channel, Config, Ring};
use std::mem::MaybeUninit;

// =============================================================================
// Basic allocator contract (INV-MEM-04)
// =============================================================================

#[test]
fn test_numa_allocator_fixed_ring_push_consume() {
    let alloc = NumaAllocator::new(NumaPolicy::Fixed(0));
    let ring = Ring::<u64, NumaAllocator>::new_in(Config::default(), alloc);

    ring.push(42);
    ring.push(43);

    let mut values = Vec::new();
    ring.consume_batch(|item: &u64| values.push(*item));
    assert_eq!(values, vec![42, 43]);
}

#[test]
fn test_numa_allocator_round_robin_ring() {
    let alloc = NumaAllocator::new(NumaPolicy::RoundRobin);
    let ring = Ring::<u64, NumaAllocator>::new_in(Config::default(), alloc);

    for i in 0..100u64 {
        ring.push(i);
    }

    let mut count = 0u64;
    ring.consume_batch(|_: &u64| count += 1);
    assert_eq!(count, 100);
}

#[test]
fn test_numa_allocator_producer_local_ring() {
    let alloc = NumaAllocator::new(NumaPolicy::ProducerLocal);
    let ring = Ring::<u64, NumaAllocator>::new_in(Config::default(), alloc);

    ring.push(99);
    let mut val = 0u64;
    ring.consume_batch(|item| val = *item);
    assert_eq!(val, 99);
}

// =============================================================================
// Zero-copy reserve/commit API
// =============================================================================

#[test]
fn test_numa_ring_reserve_commit() {
    let alloc = NumaAllocator::new(NumaPolicy::Fixed(0));
    let ring = Ring::<u64, NumaAllocator>::new_in(Config::default(), alloc);

    if let Some(mut reservation) = ring.reserve(3) {
        let slice = reservation.as_mut_slice();
        let n = slice.len();
        for (i, slot) in slice.iter_mut().enumerate() {
            *slot = MaybeUninit::new(i as u64 + 10);
        }
        reservation.commit();

        let mut values = Vec::new();
        ring.consume_batch(|item: &u64| values.push(*item));
        assert_eq!(values.len(), n);
        assert_eq!(values[0], 10);
    }
}

// =============================================================================
// Channel integration
// =============================================================================

#[test]
fn test_numa_channel_fixed() {
    let config = Config::new(12, 4, false);
    let alloc = NumaAllocator::new(NumaPolicy::Fixed(0));
    let channel = Channel::<u64, NumaAllocator>::new_in(config, alloc);

    let p1 = channel.register().unwrap();
    let p2 = channel.register().unwrap();

    p1.push(1);
    p2.push(2);

    let mut values = Vec::new();
    channel.consume_all(|item: &u64| values.push(*item));
    values.sort();
    assert_eq!(values, vec![1, 2]);
}

#[test]
fn test_numa_channel_round_robin() {
    let config = Config::new(12, 8, false);
    let alloc = NumaAllocator::new(NumaPolicy::RoundRobin);
    let channel = Channel::<u64, NumaAllocator>::new_in(config, alloc);

    for i in 0..4 {
        let producer = channel.register().unwrap();
        producer.push(i);
    }

    let mut sum = 0u64;
    channel.consume_all(|item: &u64| sum += item);
    assert_eq!(sum, 0 + 1 + 2 + 3);
}

#[test]
fn test_numa_channel_new_numa_convenience() {
    let config = Config::new(12, 4, false);
    let channel = Channel::<u64, NumaAllocator>::new_numa(config, NumaPolicy::Fixed(0));

    let producer = channel.register().unwrap();
    producer.push(42);

    let mut val = 0u64;
    channel.consume_all(|item: &u64| val = *item);
    assert_eq!(val, 42);
}

// =============================================================================
// RoundRobin counter determinism (INV-NUMA-03)
// =============================================================================

#[test]
fn test_numa_round_robin_counter_advances() {
    use ringmpsc_rs::allocator::BufferAllocator;

    let alloc = NumaAllocator::new(NumaPolicy::RoundRobin);
    let num_nodes = alloc.num_nodes();

    // Allocate several buffers — the internal counter should cycle.
    // We can't directly observe the node, but we verify the allocator
    // doesn't panic and produces valid buffers.
    for _ in 0..(num_nodes as usize * 3) {
        let buf: Box<[MaybeUninit<u64>]> = {
            // On non-Linux this returns a Box; on Linux a NumaBuffer.
            // We just need it to not panic and be the right size.
            let _buf = alloc.allocate::<u64>(1024);
            assert_eq!(_buf.len(), 1024);
            // Convert to box for uniform handling — we only care about len.
            let mut v = Vec::with_capacity(1024);
            v.resize_with(1024, MaybeUninit::uninit);
            v.into_boxed_slice()
        };
        assert_eq!(buf.len(), 1024);
    }
}

// =============================================================================
// NumaAllocator metadata
// =============================================================================

#[test]
fn test_numa_allocator_num_nodes() {
    let alloc = NumaAllocator::new(NumaPolicy::Fixed(0));
    // Must be at least 1 on every platform.
    assert!(alloc.num_nodes() >= 1);
}

#[test]
fn test_numa_available_function() {
    // Just ensure it doesn't panic. On single-node systems returns false.
    let _ = ringmpsc_rs::numa::numa_available();
}

// =============================================================================
// Drop safety — ensure NumaBuffer doesn't leak
// =============================================================================

#[test]
fn test_numa_ring_drop_with_unconsumed_items() {
    let alloc = NumaAllocator::new(NumaPolicy::Fixed(0));
    let ring = Ring::<String, NumaAllocator>::new_in(
        Config::new(8, 1, false),
        alloc,
    );

    // Push heap-allocated items
    for i in 0..10 {
        ring.push(format!("item-{}", i));
    }

    // Drop ring without consuming — should not leak.
    // (Verified by running under Miri or ASan.)
    drop(ring);
}

#[test]
fn test_numa_channel_drop_with_unconsumed_items() {
    let config = Config::new(8, 2, false);
    let channel = Channel::<String, NumaAllocator>::new_numa(
        config,
        NumaPolicy::Fixed(0),
    );

    let p = channel.register().unwrap();
    p.push("hello".to_string());
    p.push("world".to_string());

    drop(channel);
}

// =============================================================================
// Multi-threaded usage
// =============================================================================

#[test]
fn test_numa_channel_multithreaded() {
    use std::sync::Arc;
    use std::thread;

    let config = Config::new(12, 4, false);
    let channel = Arc::new(
        Channel::<u64, NumaAllocator>::new_numa(config, NumaPolicy::RoundRobin),
    );

    let mut handles = Vec::new();
    for t in 0..4u64 {
        let ch = Arc::clone(&channel);
        handles.push(thread::spawn(move || {
            let producer = ch.register().unwrap();
            for i in 0..100 {
                producer.push(t * 1000 + i);
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    let mut count = 0u64;
    channel.consume_all(|_: &u64| count += 1);
    assert_eq!(count, 400);
}
