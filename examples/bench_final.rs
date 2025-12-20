use ringmpsc_rs::{Channel, Config};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

const MSG_PER_PRODUCER: u64 = 500_000_000; // 500M messages per producer
const BATCH_SIZE: usize = 32_768; // 32K batch

fn run_benchmark(num_producers: usize) {
    let config = Config::new(16, num_producers.max(16), false); // 64K ring
    let channel = Arc::new(Channel::<u32>::new(config));
    
    // Consumer counts (one per consumer thread)
    let consumer_counts: Vec<Arc<AtomicU64>> = (0..num_producers)
        .map(|_| Arc::new(AtomicU64::new(0)))
        .collect();
    
    let start = Instant::now();
    
    // Spawn N consumer threads (one per ring) - like Zig version
    let mut consumer_handles = vec![];
    for id in 0..num_producers {
        let ch = Arc::clone(&channel);
        let count = Arc::clone(&consumer_counts[id]);
        
        let handle = thread::spawn(move || {
            // Get dedicated ring for this consumer
            let ring = ch.get_ring(id).expect("Invalid ring ID");
            let mut consumed = 0u64;
            
            loop {
                let n = ring.consume_batch(|_item| {
                    // Process item (no-op for benchmark)
                }) as u64;
                
                consumed += n;
                
                if n == 0 {
                    // Check if ring is closed and empty
                    if ring.is_closed() && ring.is_empty() {
                        break;
                    }
                    thread::yield_now();
                }
            }
            
            count.store(consumed, Ordering::Release);
        });
        consumer_handles.push(handle);
    }
    
    // Spawn N producer threads
    let mut producer_handles = vec![];
    for _ in 0..num_producers {
        let ch = Arc::clone(&channel);
        
        let handle = thread::spawn(move || {
            let producer = ch.register().unwrap();
            let mut sent = 0u64;
            
            while sent < MSG_PER_PRODUCER {
                let remaining = MSG_PER_PRODUCER - sent;
                let want = BATCH_SIZE.min(remaining as usize);
                
                loop {
                    if let Some(mut r) = producer.reserve(want) {
                        let slice = r.as_mut_slice();
                        let n = slice.len();
                        
                        // Write data (4-way unroll like Zig)
                        let mut i = 0;
                        while i + 4 <= n {
                            slice[i] = (sent + i as u64) as u32;
                            slice[i + 1] = (sent + i as u64 + 1) as u32;
                            slice[i + 2] = (sent + i as u64 + 2) as u32;
                            slice[i + 3] = (sent + i as u64 + 3) as u32;
                            i += 4;
                        }
                        while i < n {
                            slice[i] = (sent + i as u64) as u32;
                            i += 1;
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
    
    // Wait for all producers
    for handle in producer_handles {
        handle.join().unwrap();
    }
    
    // Close all rings to signal consumers
    channel.close();
    
    // Wait for all consumers
    for handle in consumer_handles {
        handle.join().unwrap();
    }
    
    let duration = start.elapsed();
    
    // Sum all consumer counts
    let total_consumed: u64 = consumer_counts
        .iter()
        .map(|c| c.load(Ordering::Acquire))
        .sum();
    
    let throughput = total_consumed as f64 / duration.as_secs_f64();
    
    println!(
        "| {:4} | {:12.2} | {:8.1} |",
        format!("{}P{}C", num_producers, num_producers),
        throughput / 1_000_000_000.0,
        total_consumed as f64 / 1_000_000.0
    );
}

fn main() {
    println!();
    println!("═══════════════════════════════════════════════════════════════════════════════");
    println!("║                   RINGMPSC-RS - THROUGHPUT BENCHMARK                        ║");
    println!("═══════════════════════════════════════════════════════════════════════════════");
    println!("Config:   {}M msgs/producer, batch={}, ring=64K slots", MSG_PER_PRODUCER / 1_000_000, BATCH_SIZE);
    println!("Pattern:  N producers × N consumers (dedicated ring per consumer)\n");
    
    println!("┌──────┬──────────────┬──────────┐");
    println!("│ Conf │ Throughput   │ Total    │");
    println!("├──────┼──────────────┼──────────┤");
    
    // Test configurations: N producers, N consumers (SPMC pattern)
    run_benchmark(1); // 1P1C
    run_benchmark(2); // 2P2C
    run_benchmark(4); // 4P4C
    run_benchmark(6); // 6P6C
    run_benchmark(8); // 8P8C
    
    println!("└──────┴──────────────┴──────────┘");
    println!("\nB/s = billion messages per second");
    println!("Pattern matches Zig implementation: each consumer reads from dedicated ring");
    println!("═══════════════════════════════════════════════════════════════════════════════\n");
}
