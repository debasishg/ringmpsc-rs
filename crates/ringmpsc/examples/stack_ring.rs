//! Stack-allocated ring buffer examples.
//!
//! This example demonstrates `StackRing` and `StackChannel` — stack-allocated
//! variants of the lock-free SPSC/MPSC primitives that achieve 2-4x higher
//! throughput by eliminating heap pointer indirection.
//!
//! Run with: cargo run --release --features stack-ring --example stack_ring

#[cfg(not(feature = "stack-ring"))]
fn main() {
    eprintln!("This example requires the 'stack-ring' feature.");
    eprintln!("Run with: cargo run --release --features stack-ring --example stack_ring");
}

#[cfg(feature = "stack-ring")]
use ringmpsc_rs::{StackChannel, StackRing};
#[cfg(feature = "stack-ring")]
use std::thread;
#[cfg(feature = "stack-ring")]
use std::time::Instant;

#[cfg(feature = "stack-ring")]
const MSG_COUNT: u64 = 10_000_000;

#[cfg(feature = "stack-ring")]
fn main() {
    println!("=== Stack-Allocated Ring Buffer Examples ===\n");

    example_spsc_basic();
    example_spsc_throughput();
    example_mpsc_basic();
    example_mpsc_throughput();
}

/// Basic SPSC usage with StackRing
#[cfg(feature = "stack-ring")]
fn example_spsc_basic() {
    println!("1. Basic SPSC (StackRing)");
    println!("   ----------------------");

    // Create a stack-allocated ring with 1024 slots
    let ring: StackRing<u64, 1024> = StackRing::new();

    // Producer: write 10 items using reserve/commit
    unsafe {
        for batch_start in (0..10).step_by(5) {
            if let Some((ptr, len)) = ring.reserve(5) {
                for i in 0..len {
                    *ptr.add(i) = (batch_start + i) as u64 * 10;
                }
                ring.commit(len);
            }
        }
    }

    println!("   Produced 10 items");

    // Consumer: read all items
    let mut values = Vec::new();
    unsafe {
        ring.consume_batch(|v| values.push(*v));
    }

    println!("   Consumed: {:?}", values);
    println!();
}

/// SPSC throughput demonstration
#[cfg(feature = "stack-ring")]
fn example_spsc_throughput() {
    println!("2. SPSC Throughput (StackRing)");
    println!("   ---------------------------");

    // Use Box to put the large ring on heap (but buffer is still inline!)
    let ring = Box::new(StackRing::<u64, 65536>::new());
    let ring_ptr = Box::into_raw(ring);

    // SAFETY: We control the lifetime - ring lives until both threads complete
    let ring_ref: &'static StackRing<u64, 65536> = unsafe { &*ring_ptr };

    let start = Instant::now();

    // Producer thread
    let producer = thread::spawn(move || {
        let mut sent = 0u64;
        while sent < MSG_COUNT {
            unsafe {
                if let Some((ptr, len)) = ring_ref.reserve(1024) {
                    for i in 0..len {
                        *ptr.add(i) = sent + i as u64;
                    }
                    ring_ref.commit(len);
                    sent += len as u64;
                } else {
                    std::hint::spin_loop();
                }
            }
        }
    });

    // Consumer on main thread
    let mut received = 0u64;
    while received < MSG_COUNT {
        let consumed = unsafe {
            ring_ref.consume_batch(|_| {
                // Just count, don't process
            })
        };
        received += consumed as u64;
        if consumed == 0 {
            std::hint::spin_loop();
        }
    }

    producer.join().unwrap();
    let elapsed = start.elapsed();

    // Cleanup
    unsafe {
        drop(Box::from_raw(ring_ptr));
    }

    let throughput = MSG_COUNT as f64 / elapsed.as_secs_f64() / 1e9;
    println!("   {} messages in {:?}", MSG_COUNT, elapsed);
    println!("   Throughput: {:.2} Gelem/s", throughput);
    println!();
}

/// Basic MPSC usage with StackChannel
#[cfg(feature = "stack-ring")]
fn example_mpsc_basic() {
    println!("3. Basic MPSC (StackChannel)");
    println!("   -------------------------");

    // 256 slots per ring, max 4 producers
    let channel: StackChannel<String, 256, 4> = StackChannel::new();

    // Register 3 producers
    let p1 = channel.register().unwrap();
    let p2 = channel.register().unwrap();
    let p3 = channel.register().unwrap();

    println!("   Registered {} producers", channel.producer_count());

    // Each producer sends a message
    p1.push("Hello from producer 1".to_string());
    p2.push("Hello from producer 2".to_string());
    p3.push("Hello from producer 3".to_string());

    // Consume all messages
    let mut messages = Vec::new();
    channel.consume_all_owned(|msg| messages.push(msg));

    for msg in &messages {
        println!("   Received: {}", msg);
    }
    println!();
}

/// MPSC throughput with multiple producer threads
#[cfg(feature = "stack-ring")]
fn example_mpsc_throughput() {
    println!("4. MPSC Throughput (StackChannel, 4 producers)");
    println!("   -------------------------------------------");

    const NUM_PRODUCERS: usize = 4;
    let msg_per_producer = MSG_COUNT / NUM_PRODUCERS as u64;

    // Put channel on heap for 'static lifetime, but rings are still inline
    let channel = Box::new(StackChannel::<u64, 65536, 4>::new());
    let channel_ptr = Box::into_raw(channel);
    let channel_ref: &'static StackChannel<u64, 65536, 4> = unsafe { &*channel_ptr };

    let start = Instant::now();

    // Spawn producer threads
    let handles: Vec<_> = (0..NUM_PRODUCERS)
        .map(|id| {
            let producer = channel_ref.register().unwrap();
            thread::spawn(move || {
                let base = id as u64 * msg_per_producer;
                let mut sent = 0u64;
                while sent < msg_per_producer {
                    unsafe {
                        if let Some((ptr, len)) = producer.reserve(512) {
                            for i in 0..len {
                                *ptr.add(i) = base + sent + i as u64;
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

    // Consumer on main thread
    let mut received = 0u64;
    while received < MSG_COUNT {
        let consumed = channel_ref.consume_all(|_| {});
        received += consumed as u64;
        if consumed == 0 {
            std::hint::spin_loop();
        }
    }

    for h in handles {
        h.join().unwrap();
    }

    let elapsed = start.elapsed();

    // Cleanup
    unsafe {
        drop(Box::from_raw(channel_ptr));
    }

    let throughput = MSG_COUNT as f64 / elapsed.as_secs_f64() / 1e9;
    println!("   {} messages in {:?}", MSG_COUNT, elapsed);
    println!("   Throughput: {:.2} Gelem/s ({} producers)", throughput, NUM_PRODUCERS);
    println!();
}
