use ringmpsc_rs::{Channel, Config};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

fn main() {
    println!("RingMPSC Zero-Copy Example");
    println!("===========================\n");

    // Custom configuration for high throughput
    let config = Config::new(
        16,    // 64K slots
        8,     // 8 max producers
        false, // metrics disabled for max performance
    );

    let channel = Arc::new(Channel::<[u64; 8]>::new(config));

    const N_PRODUCERS: usize = 4;
    const BATCHES: usize = 10_000;
    const BATCH_SIZE: usize = 100;

    println!("Configuration:");
    println!("  Ring capacity: {} slots", config.capacity());
    println!("  Producers: {}", N_PRODUCERS);
    println!("  Batches per producer: {}", BATCHES);
    println!("  Batch size: {}", BATCH_SIZE);
    println!("  Total items: {}\n", N_PRODUCERS * BATCHES * BATCH_SIZE);

    let start = Instant::now();

    // Spawn producer threads
    let mut handles = vec![];
    for _id in 0..N_PRODUCERS {
        let ch = Arc::clone(&channel);
        let handle = thread::spawn(move || {
            let producer = ch.register().unwrap();
            
            let mut sent = 0;
            for _batch in 0..BATCHES {
                let mut batch_sent = 0;
                // Keep sending until we've sent BATCH_SIZE items for this batch
                while batch_sent < BATCH_SIZE {
                    let remaining = BATCH_SIZE - batch_sent;
                    if let Some(mut reservation) = producer.reserve(remaining) {
                        let slice = reservation.as_mut_slice();
                        
                        // Write directly into the ring buffer
                        for (i, item) in slice.iter_mut().enumerate() {
                            let value = (sent + i) as u64;
                            item.write([value; 8]); // 64 bytes per item
                        }
                        
                        let n = slice.len();
                        sent += n;
                        batch_sent += n;
                        reservation.commit();
                    } else {
                        // Yield if ring is full
                        thread::yield_now();
                    }
                }
            }
        });
        handles.push(handle);
    }

    // Consumer thread
    let ch = Arc::clone(&channel);
    let consumer_handle = thread::spawn(move || {
        let mut total = 0;
        let target = N_PRODUCERS * BATCHES * BATCH_SIZE;
        
        while total < target {
            // Batch consume with limited size to avoid long pauses
            let consumed = ch.consume_all_up_to(10_000, |item| {
                // Process the item (validate it)
                let _ = item[0]; // Touch the data
            });
            
            total += consumed;
            
            if consumed == 0 {
                thread::yield_now();
            }
        }
        
        total
    });

    // Wait for all producers
    for handle in handles {
        handle.join().unwrap();
    }

    // Wait for consumer
    let total = consumer_handle.join().unwrap();
    let duration = start.elapsed();

    let items_per_sec = total as f64 / duration.as_secs_f64();
    let bytes_per_sec = items_per_sec * 64.0; // 64 bytes per item

    println!("\nResults:");
    println!("  Items consumed: {}", total);
    println!("  Duration: {:.2?}", duration);
    println!("  Throughput: {:.2} million items/sec", items_per_sec / 1_000_000.0);
    println!("  Bandwidth: {:.2} GB/sec", bytes_per_sec / 1_000_000_000.0);
}
