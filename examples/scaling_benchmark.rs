use ringmpsc_rs::{Channel, Config};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

const MSG_PER_PRODUCER: u64 = 500_000_000;
const BATCH_SIZE: usize = 32_768;

fn benchmark_config(num_producers: usize) {
    println!("\n{} Producer(s) × 1 Consumer", num_producers);
    println!("{}", "=".repeat(50));
    
    let config = Config::new(16, num_producers, false);
    let channel = Arc::new(Channel::<u32>::new(config));
    
    let start = Instant::now();
    
    // Spawn producers
    let mut handles = vec![];
    for _ in 0..num_producers {
        let ch = Arc::clone(&channel);
        let handle = thread::spawn(move || {
            let producer = ch.register().unwrap();
            let mut sent = 0u64;
            
            while sent < MSG_PER_PRODUCER {
                let want = BATCH_SIZE.min((MSG_PER_PRODUCER - sent) as usize);
                
                loop {
                    if let Some(mut r) = producer.reserve(want) {
                        let slice = r.as_mut_slice();
                        for (i, item) in slice.iter_mut().enumerate() {
                            *item = (sent + i as u64) as u32;
                        }
                        sent += slice.len() as u64;
                        r.commit();
                        break;
                    }
                    thread::yield_now();
                }
            }
        });
        handles.push(handle);
    }
    
    // Consumer
    let ch = Arc::clone(&channel);
    let consumer_handle = thread::spawn(move || {
        let target = (num_producers as u64) * MSG_PER_PRODUCER;
        let mut total = 0u64;
        
        while total < target {
            let consumed = ch.consume_all_up_to(BATCH_SIZE, |_item| {
                // Process
            });
            total += consumed as u64;
            
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
    
    let total_messages = (num_producers as u64) * MSG_PER_PRODUCER;
    let throughput = total_messages as f64 / duration.as_secs_f64();
    let per_producer = throughput / num_producers as f64;
    
    println!("  Total messages:   {}", total_messages);
    println!("  Duration:         {:.2?}", duration);
    println!("  Total throughput: {:.2} B/s ({:.2} M/s)", 
             throughput / 1_000_000_000.0,
             throughput / 1_000_000.0);
    println!("  Per producer:     {:.2} M/s", per_producer / 1_000_000.0);
    println!("  Messages consumed: {}", total);
}

fn main() {
    println!("\nRingMPSC Scaling Benchmark");
    println!("==========================");
    println!("Messages per producer: {}", MSG_PER_PRODUCER);
    println!("Batch size: {}", BATCH_SIZE);
    println!("Ring capacity: 65536 slots");
    
    for num_producers in [1, 2, 4, 6, 8] {
        benchmark_config(num_producers);
    }
    
    println!("\n{}", "=".repeat(50));
    println!("Benchmark complete!");
}
