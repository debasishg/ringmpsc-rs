use ringmpsc_rs::{Channel, Config};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

fn main() {
    println!("RingMPSC Basic Example");
    println!("======================\n");

    // Create a channel with default configuration
    let channel = Arc::new(Channel::<u64>::new(Config::default()));

    const N_PRODUCERS: usize = 4;
    const ITEMS_PER_PRODUCER: usize = 1_000_000;

    println!("Configuration:");
    println!("  Producers: {}", N_PRODUCERS);
    println!("  Items per producer: {}", ITEMS_PER_PRODUCER);
    println!("  Total items: {}\n", N_PRODUCERS * ITEMS_PER_PRODUCER);

    let start = Instant::now();

    // Spawn producer threads
    let mut handles = vec![];
    for id in 0..N_PRODUCERS {
        let ch = Arc::clone(&channel);
        let handle = thread::spawn(move || {
            let producer = ch.register().unwrap();
            for i in 0..ITEMS_PER_PRODUCER {
                // Reserve space and write - retry if ring is full
                loop {
                    if let Some(mut reservation) = producer.reserve(1) {
                        reservation.as_mut_slice()[0] = (id * ITEMS_PER_PRODUCER + i) as u64;
                        reservation.commit();
                        break;
                    }
                    // Ring is full, yield to consumer
                    thread::yield_now();
                }
            }
            println!("Producer {} finished", id);
        });
        handles.push(handle);
    }

    // Consumer thread
    let ch = Arc::clone(&channel);
    let consumer_handle = thread::spawn(move || {
        let mut total = 0;
        let mut sum = 0u64;
        
        while total < N_PRODUCERS * ITEMS_PER_PRODUCER {
            // Batch consume all available items
            let consumed = ch.consume_all(|item| {
                sum += item;
            });
            total += consumed;
            
            if consumed == 0 {
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
    let duration = start.elapsed();

    println!("\nResults:");
    println!("  Items consumed: {}", total);
    println!("  Sum: {}", sum);
    println!("  Duration: {:.2?}", duration);
    println!("  Throughput: {:.2} million items/sec", 
             total as f64 / duration.as_secs_f64() / 1_000_000.0);
}
