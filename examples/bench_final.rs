use ringmpsc_rs::{Channel, Config};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

const TOTAL_MESSAGES: u64 = 500_000_000; // 500M total messages
const BATCH_SIZE: usize = 32_768; // 32K batch

fn run_benchmark(num_producers: usize) {
    let msg_per_producer = TOTAL_MESSAGES / num_producers as u64;
    // Handle remainder to ensure exact TOTAL_MESSAGES
    let remainder = TOTAL_MESSAGES % num_producers as u64;
    let actual_total = msg_per_producer * num_producers as u64 + remainder;
    
    let config = Config::new(16, num_producers.max(16), false); // 64K ring
    let channel = Arc::new(Channel::<u32>::new(config));
    
    let start = Instant::now();
    
    // Spawn producers
    let mut producer_handles = vec![];
    for id in 0..num_producers {
        let ch = Arc::clone(&channel);
        // Last producer sends the remainder too
        let to_send = if id == num_producers - 1 {
            msg_per_producer + remainder
        } else {
            msg_per_producer
        };
        
        let handle = thread::spawn(move || {
            let producer = ch.register().unwrap();
            let mut sent = 0u64;
            
            while sent < to_send {
                let remaining = to_send - sent;
                let want = BATCH_SIZE.min(remaining as usize);
                
                loop {
                    if let Some(mut r) = producer.reserve(want) {
                        let slice = r.as_mut_slice();
                        let n = slice.len();
                        
                        // Write data
                        for (i, item) in slice.iter_mut().enumerate() {
                            *item = (sent + i as u64) as u32;
                        }
                        
                        sent += n as u64;
                        r.commit();
                        break;
                    }
                    thread::yield_now();
                }
            }
        });
        producer_handles.push(handle);
    }
    
    // Single consumer (MPSC = Multi-Producer Single-Consumer)
    let ch = Arc::clone(&channel);
    let consumer_handle = thread::spawn(move || {
        let mut received = 0u64;
        
        while received < actual_total {
            let consumed = ch.consume_all_up_to(BATCH_SIZE, |_item| {
                // Process item
            }) as u64;
            
            received += consumed;
            
            if consumed == 0 {
                thread::yield_now();
            }
        }
        
        received
    });
    
    // Wait for all threads
    for handle in producer_handles {
        handle.join().unwrap();
    }
    
    let _total_consumed = consumer_handle.join().unwrap();
    
    let duration = start.elapsed();
    let throughput = actual_total as f64 / duration.as_secs_f64();
    let scaling = throughput / 8_600_000_000.0; // Relative to 1P1C baseline (8.6 B/s)
    
    println!(
        "| {:4} | {:12.1} | {:5.1}x |",
        format!("{}P1C", num_producers),
        throughput / 1_000_000_000.0,
        scaling
    );
}

fn main() {
    println!("\nRingMPSC Final Benchmark");
    println!("========================");
    println!("Total messages: {} ({:.1}M)", TOTAL_MESSAGES, TOTAL_MESSAGES as f64 / 1_000_000.0);
    println!("Batch size: {}", BATCH_SIZE);
    println!("Ring size: 64K slots\n");
    
    println!("| Config | Throughput   | Scaling |");
    println!("|--------|--------------|---------|");
    
    // Test configurations: N producers, 1 consumer (MPSC)
    run_benchmark(1); // 1P1C - baseline
    run_benchmark(2); // 2P1C
    run_benchmark(4); // 4P1C
    run_benchmark(6); // 6P1C
    run_benchmark(8); // 8P1C
    
    println!("\nBenchmark complete!");
    println!("\nNote: Scaling is relative to 1P1C baseline (8.6 B/s expected)");
    println!("      Higher values indicate better parallel scalability");
}
