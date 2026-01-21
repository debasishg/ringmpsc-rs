//! Async Multi-Producer Span Generator
//!
//! Demonstrates structured async concurrency patterns for generating spans:
//! - `JoinSet` for managing dynamic producer task lifecycles
//! - `watch` channel for graceful shutdown signaling
//! - Decoupled rate limiting via `RateLimiter` trait
//! - Per-producer FIFO ordering with varied generation rates
//!
//! Run with: `cargo run --example span_generator`

use rand::Rng;
use span_collector::{
    AsyncCollectorConfig, AsyncSpanCollector, AttributeValue,
    IntervalRateLimiter, RateLimiter, Span, SpanKind, SpanStatus, StdoutExporter,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::task::JoinSet;

/// Statistics collected from each producer task.
#[derive(Debug)]
struct ProducerStats {
    producer_id: usize,
    spans_sent: u64,
    duration: Duration,
    target_rate: Option<f64>,
}

impl ProducerStats {
    fn effective_rate(&self) -> f64 {
        if self.duration.as_secs_f64() > 0.0 {
            self.spans_sent as f64 / self.duration.as_secs_f64()
        } else {
            0.0
        }
    }
}

/// Configuration for a single producer task.
struct ProducerConfig {
    id: usize,
    rate_per_sec: f64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Async Multi-Producer Span Generator ===\n");

    // --- Setup Collector ---
    let exporter = Arc::new(StdoutExporter::new(false)); // non-verbose for cleaner output
    let config = AsyncCollectorConfig {
        consumer_interval: Duration::from_millis(50), // Fast polling for demo
        ..Default::default()
    };

    println!("Collector Configuration:");
    println!("  Ring buffer size: {} spans", 1 << config.collector_config.ring_bits);
    println!("  Max producers: {}", config.collector_config.max_producers);
    println!("  Consumer interval: {:?}\n", config.consumer_interval);

    let collector = Arc::new(AsyncSpanCollector::new(config, exporter).await);

    // --- Setup Shutdown Signal ---
    // watch channel: sender broadcasts shutdown, receivers check for signal
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // --- Configure Producers with Varied Rates ---
    let producer_configs = vec![
        ProducerConfig { id: 0, rate_per_sec: 50.0 },   // Slow producer
        ProducerConfig { id: 1, rate_per_sec: 100.0 },  // Medium producer
        ProducerConfig { id: 2, rate_per_sec: 200.0 },  // Fast producer
        ProducerConfig { id: 3, rate_per_sec: 500.0 },  // Very fast producer
    ];

    println!("Starting {} producer tasks with varied rates:", producer_configs.len());
    for cfg in &producer_configs {
        println!("  Producer {}: {} spans/sec", cfg.id, cfg.rate_per_sec);
    }
    println!();

    // --- Spawn Producer Tasks with JoinSet ---
    let mut join_set: JoinSet<Result<ProducerStats, String>> = JoinSet::new();

    for cfg in producer_configs {
        let collector_clone = Arc::clone(&collector);
        let shutdown_rx_clone = shutdown_rx.clone();
        let rate_limiter = Box::new(IntervalRateLimiter::from_rate(cfg.rate_per_sec));

        join_set.spawn(async move {
            producer_task(
                cfg.id,
                collector_clone,
                rate_limiter,
                shutdown_rx_clone,
            )
            .await
        });
    }

    // --- Wait for Shutdown Signal ---
    println!("Generators running. Press Ctrl+C to stop, or wait 10 seconds...\n");

    let shutdown_timeout = Duration::from_secs(10);
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("\nReceived Ctrl+C, initiating graceful shutdown...");
        }
        _ = tokio::time::sleep(shutdown_timeout) => {
            println!("\nTimeout reached, initiating graceful shutdown...");
        }
    }

    // Broadcast shutdown signal to all producers
    shutdown_tx.send(true).expect("Failed to send shutdown signal");

    // --- Await All Producer Tasks ---
    println!("Waiting for producer tasks to finish...\n");

    let mut all_stats = Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(Ok(stats)) => {
                println!(
                    "Producer {} finished: {} spans in {:.2}s ({:.1} spans/sec, target: {:.1})",
                    stats.producer_id,
                    stats.spans_sent,
                    stats.duration.as_secs_f64(),
                    stats.effective_rate(),
                    stats.target_rate.unwrap_or(f64::INFINITY),
                );
                all_stats.push(stats);
            }
            Ok(Err(e)) => {
                eprintln!("Producer task failed: {}", e);
            }
            Err(e) => {
                eprintln!("Task join error: {}", e);
            }
        }
    }

    // --- Print Summary ---
    println!("\n=== Generation Summary ===");
    let total_spans: u64 = all_stats.iter().map(|s| s.spans_sent).sum();
    let max_duration = all_stats
        .iter()
        .map(|s| s.duration)
        .max()
        .unwrap_or(Duration::ZERO);

    println!("Total spans generated: {}", total_spans);
    println!("Total duration: {:.2}s", max_duration.as_secs_f64());
    if max_duration.as_secs_f64() > 0.0 {
        println!(
            "Aggregate throughput: {:.1} spans/sec",
            total_spans as f64 / max_duration.as_secs_f64()
        );
    }

    // --- Graceful Collector Shutdown ---
    println!("\nShutting down collector (draining remaining spans)...");

    // Brief pause to let consumer process final spans
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Must unwrap Arc before shutdown (no other references should exist)
    let collector = Arc::try_unwrap(collector)
        .map_err(|_| "Failed to unwrap collector Arc - references still held")?;
    collector.shutdown().await?;

    println!("Shutdown complete!");
    Ok(())
}

/// Async producer task that generates random spans at a controlled rate.
///
/// Runs until the shutdown signal is received, then exits gracefully.
async fn producer_task(
    producer_id: usize,
    collector: Arc<AsyncSpanCollector>,
    mut rate_limiter: Box<dyn RateLimiter>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<ProducerStats, String> {
    // Register this producer with the collector (gets dedicated SPSC ring)
    let producer = collector
        .register_producer()
        .await
        .map_err(|e| format!("Producer {} registration failed: {}", producer_id, e))?;

    let target_rate = rate_limiter.target_rate();
    let start_time = Instant::now();
    let mut sequence = 0u64;

    loop {
        // Check for shutdown signal (non-blocking)
        if *shutdown_rx.borrow_and_update() {
            break;
        }

        // Rate limiting - wait for next permitted operation
        rate_limiter.wait().await;

        // Generate and submit a random span
        let span = generate_random_span(producer_id, sequence);

        // Submit using async backpressure (waits on Notify if ring full)
        if let Err(e) = producer.submit_span(span).await {
            // Log error but continue - span collector may have closed
            eprintln!("Producer {} submit error: {}", producer_id, e);
            break;
        }

        sequence += 1;

        // Periodically yield to ensure fair scheduling (every 100 spans)
        if sequence.is_multiple_of(100) {
            tokio::task::yield_now().await;
        }
    }

    Ok(ProducerStats {
        producer_id,
        spans_sent: sequence,
        duration: start_time.elapsed(),
        target_rate,
    })
}

/// Generate a random span with realistic telemetry attributes.
fn generate_random_span(producer_id: usize, sequence: u64) -> Span {
    let mut rng = rand::thread_rng();

    // Operation names simulating various service operations
    let operations = [
        "http.request",
        "http.response",
        "db.query",
        "db.connect",
        "cache.get",
        "cache.set",
        "queue.publish",
        "queue.consume",
        "rpc.call",
        "auth.validate",
    ];

    // Generate unique IDs incorporating producer_id for traceability
    let trace_id: u128 = rng.gen();
    let span_id = ((producer_id as u64) << 48) | sequence;
    let parent_span_id: u64 = if rng.gen_bool(0.3) {
        0 // 30% are root spans
    } else {
        rng.gen() // 70% have a parent
    };

    let operation = operations[rng.gen_range(0..operations.len())];
    let kind = match rng.gen_range(0..5) {
        0 => SpanKind::Server,
        1 => SpanKind::Client,
        2 => SpanKind::Producer,
        3 => SpanKind::Consumer,
        _ => SpanKind::Internal,
    };

    let mut span = Span::new(
        trace_id,
        span_id,
        parent_span_id,
        operation.to_string(),
        kind,
    );

    // Add realistic attributes
    span.set_attribute(
        "producer.id".to_string(),
        AttributeValue::Int(producer_id as i64),
    );
    span.set_attribute(
        "span.sequence".to_string(),
        AttributeValue::Int(sequence as i64),
    );
    span.set_attribute(
        "service.name".to_string(),
        AttributeValue::String(format!("service-{}", producer_id)),
    );

    // Add operation-specific attributes
    match operation {
        "http.request" | "http.response" => {
            span.set_attribute(
                "http.method".to_string(),
                AttributeValue::String(["GET", "POST", "PUT", "DELETE"][rng.gen_range(0..4)].to_string()),
            );
            span.set_attribute(
                "http.status_code".to_string(),
                AttributeValue::Int([200, 201, 400, 404, 500][rng.gen_range(0..5)]),
            );
        }
        "db.query" | "db.connect" => {
            span.set_attribute(
                "db.system".to_string(),
                AttributeValue::String(["postgresql", "mysql", "redis"][rng.gen_range(0..3)].to_string()),
            );
        }
        "cache.get" | "cache.set" => {
            span.set_attribute("cache.hit".to_string(), AttributeValue::Bool(rng.gen_bool(0.7)));
        }
        _ => {}
    }

    // Finish span with random status (mostly OK)
    let status = if rng.gen_bool(0.95) {
        SpanStatus::Ok
    } else {
        span.set_attribute("error".to_string(), AttributeValue::Bool(true));
        span.set_attribute(
            "error.message".to_string(),
            AttributeValue::String("Simulated error".to_string()),
        );
        SpanStatus::Error
    };
    span.finish(status);

    span
}
