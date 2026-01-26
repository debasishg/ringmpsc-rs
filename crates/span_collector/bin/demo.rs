//! # Distributed Tracing Span Collector Demo
//!
//! A complete end-to-end demonstration showcasing all features of the span collector.
//!
//! **Note**: This collector uses an OpenTelemetry-compatible data model (trace_id, span_id,
//! attributes, etc.) but does **not** implement the OTLP protocol. The `SpanExporter` trait
//! is designed to be extensible - an OTLP exporter could be added as a future enhancement.
//!
//! ## Features Demonstrated
//!
//! ### 1. Resilient Export Pipeline
//! - `RetryingExporter` with exponential backoff (3 retries, 10ms initial, 2x multiplier)
//! - `CircuitBreakerExporter` with configurable thresholds (5 failures to open, 2s reset)
//! - `RateLimitedExporter` using `IntervalRateLimiter` (100 exports/sec)
//! - Builder pattern via `ResilientExporterBuilder`
//!
//! ### 2. Native Async Traits (Rust 2024 Edition)
//! - Custom `SpanExporter` implementation using `impl Future<...> + Send`
//! - `SimulatedBackendExporter` with configurable failure rate and latency
//! - Demonstrates the pattern without `#[async_trait]` macro
//! - `if-let` chains for concise conditional error handling
//!
//! ### 3. Multi-Producer Workflow
//! - 8 concurrent producer tasks (4 in quick mode)
//! - Each generating spans with realistic attributes
//! - Rate-limited span generation per producer
//!
//! ### 4. OpenTelemetry-Compatible Span Data Model
//! - Service-level attributes (`service.name`, `service.instance.id`)
//! - Operation-specific attributes (HTTP, DB, gRPC, cache, messaging)
//! - Parent-child relationships (80% have parents)
//! - 10% error rate with error attributes
//!
//! ### 5. Backpressure & Metrics
//! - Live metrics display during execution
//! - Final statistics dashboard with throughput calculations
//! - Ring buffer status monitoring
//!
//! ### 6. Graceful Shutdown
//! - Proper drain of remaining spans
//! - Final batch flush before exit
//!
//! ## Running
//!
//! ```bash
//! # Quick mode (4 producers, 25 spans each)
//! cargo run -p span_collector --bin demo --release -- --quick
//!
//! # Full mode (8 producers, 100 spans each)
//! cargo run -p span_collector --bin demo --release
//!
//! # Verbose mode (see individual producer completions)
//! cargo run -p span_collector --bin demo --release -- --verbose
//! ```

use span_collector::{
    AsyncCollectorConfig, AsyncSpanCollector, AttributeValue, BatchConfig,
    CircuitBreakerConfig, CollectorConfig, CollectorMetrics,
    IntervalRateLimiter, RateLimitedExporter, ResilientExporterBuilder,
    RetryConfig, RateLimiter, Span, SpanBatch, SpanExporter, SpanKind, SpanStatus,
    ExportError, SpanExporterBoxed,
};
use std::future::Future;
use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// =============================================================================
// CUSTOM EXPORTERS (demonstrating the SpanExporter trait)
// =============================================================================

/// A simulated backend exporter that occasionally fails (for demonstrating resilience).
///
/// This shows how to implement the native async trait `SpanExporter`.
struct SimulatedBackendExporter {
    /// Probability of failure (0.0 - 1.0)
    failure_rate: f64,
    /// Counter for total export attempts
    export_attempts: AtomicU64,
    /// Counter for successful exports
    successful_exports: AtomicU64,
    /// Counter for failed exports
    failed_exports: AtomicU64,
    /// Simulated latency per export
    latency: Duration,
    /// Name for identification
    name: String,
}

impl SimulatedBackendExporter {
    fn new(name: &str, failure_rate: f64, latency: Duration) -> Self {
        Self {
            failure_rate,
            export_attempts: AtomicU64::new(0),
            successful_exports: AtomicU64::new(0),
            failed_exports: AtomicU64::new(0),
            latency,
            name: name.to_string(),
        }
    }

    fn stats(&self) -> (u64, u64, u64) {
        (
            self.export_attempts.load(Ordering::Relaxed),
            self.successful_exports.load(Ordering::Relaxed),
            self.failed_exports.load(Ordering::Relaxed),
        )
    }
}

/// Native async trait implementation (Rust 2024 edition - no macro needed!)
impl SpanExporter for SimulatedBackendExporter {
    // Note: `async fn` in trait with `impl Future<...> + Send` for multi-threaded runtime
    fn export(&self, batch: SpanBatch) -> impl Future<Output = Result<(), ExportError>> + Send {
        // Capture values for the async block
        let span_count = batch.spans.len();
        let failure_rate = self.failure_rate;
        let latency = self.latency;

        // Update attempt counter before async work
        self.export_attempts.fetch_add(1, Ordering::Relaxed);

        // Return a future (this is what native async traits compile to)
        async move {
            // Simulate network latency
            tokio::time::sleep(latency).await;

            // Simulate random failures
            let should_fail = rand_simple() < failure_rate;

            if should_fail {
                Err(ExportError::Transport(format!(
                    "Simulated backend failure (batch of {} spans)",
                    span_count
                )))
            } else {
                Ok(())
            }
        }
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// Simple pseudo-random number generator (no external dependency)
fn rand_simple() -> f64 {
    use std::sync::atomic::AtomicU64;
    static SEED: AtomicU64 = AtomicU64::new(12345);

    let old = SEED.load(Ordering::Relaxed);
    let new = old.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    SEED.store(new, Ordering::Relaxed);

    (new as f64) / (u64::MAX as f64)
}

// =============================================================================
// MAIN APPLICATION
// =============================================================================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    print_banner();

    // Parse command line arguments for demo mode
    let args: Vec<String> = std::env::args().collect();
    let verbose = args.contains(&"--verbose".to_string());
    let quick = args.contains(&"--quick".to_string());

    // Configuration based on mode
    let (num_producers, spans_per_producer) = if quick {
        (4, 25)
    } else {
        (8, 100)
    };

    println!("📋 Demo Configuration:");
    println!("   Mode: {}", if quick { "quick" } else { "full" });
    println!("   Verbose output: {}", verbose);
    println!("   Producers: {}", num_producers);
    println!("   Spans per producer: {}", spans_per_producer);
    println!();

    // =========================================================================
    // PHASE 1: Setup Resilient Export Pipeline
    // =========================================================================

    println!("🔧 Phase 1: Building Resilient Export Pipeline\n");

    // Create the base exporter (simulates an unreliable backend)
    let backend = Arc::new(SimulatedBackendExporter::new(
        "simulated-jaeger",
        0.15,  // 15% failure rate
        Duration::from_millis(5),  // 5ms simulated latency
    ));
    let backend_stats = Arc::clone(&backend);

    // Build resilient pipeline using the builder pattern
    //
    // Composition order (outer to inner):
    // 1. Rate Limiter (100 exports/sec max)
    // 2. Circuit Breaker (fail fast when backend unhealthy)
    // 3. Retry (3 attempts with exponential backoff)
    // 4. Base Exporter (simulated backend)

    let retry_config = RetryConfig {
        max_retries: 3,
        initial_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(100),
        backoff_multiplier: 2.0,
    };

    let circuit_config = CircuitBreakerConfig {
        failure_threshold: 5,
        reset_timeout: Duration::from_secs(2),
        success_threshold: 2,
    };

    // Using the builder for retry + circuit breaker
    let resilient_exporter = ResilientExporterBuilder::new(SimulatedBackendExporter::new(
        "jaeger-backend",
        0.15,
        Duration::from_millis(5),
    ))
    .with_retry(retry_config.clone())
    .with_circuit_breaker(circuit_config.clone())
    .build_with_retry_and_circuit_breaker();

    // Add rate limiting on top
    let rate_limiter = IntervalRateLimiter::from_rate(100.0); // 100 exports/sec
    let rate_limited_exporter = RateLimitedExporter::new(resilient_exporter, rate_limiter);

    // Wrap as Arc<dyn SpanExporterBoxed> for the collector
    let final_exporter: Arc<dyn SpanExporterBoxed> =
        Arc::new(rate_limited_exporter);

    println!("   ✅ Retry: {} attempts, {}ms initial delay, {}x backoff",
        retry_config.max_retries,
        retry_config.initial_delay.as_millis(),
        retry_config.backoff_multiplier);
    println!("   ✅ Circuit Breaker: {} failures to open, {}s reset timeout",
        circuit_config.failure_threshold,
        circuit_config.reset_timeout.as_secs());
    println!("   ✅ Rate Limiter: 100 exports/sec max");
    println!();

    // =========================================================================
    // PHASE 2: Configure and Create Async Collector
    // =========================================================================

    println!("🔧 Phase 2: Configuring Async Collector\n");

    let collector_config = CollectorConfig {
        ring_bits: 12,        // 4K spans per ring buffer
        max_producers: 16,    // Support up to 16 concurrent producers
        enable_metrics: true, // Track submission/consumption metrics
    };

    let batch_config = BatchConfig {
        batch_size_limit: 500,                   // Flush when 500 spans accumulated
        batch_timeout: Duration::from_millis(200), // Or every 200ms
    };

    let async_config = AsyncCollectorConfig {
        collector_config: collector_config.clone(),
        batch_config: batch_config.clone(),
        consumer_interval: Duration::from_millis(50), // Poll 20 times/sec
        max_consume_per_poll: 1000,
    };

    println!("   Ring Buffer: {} slots ({} KB per ring)",
        1 << collector_config.ring_bits,
        (1 << collector_config.ring_bits) * std::mem::size_of::<Span>() / 1024);
    println!("   Max Producers: {}", collector_config.max_producers);
    println!("   Batch Size: {} spans", batch_config.batch_size_limit);
    println!("   Batch Timeout: {:?}", batch_config.batch_timeout);
    println!("   Consumer Poll: {:?}", async_config.consumer_interval);
    println!();

    // Create the async collector
    let collector = Arc::new(AsyncSpanCollector::new(async_config, final_exporter).await);
    let metrics = Arc::clone(collector.metrics());

    // =========================================================================
    // PHASE 3: Spawn Producer Tasks
    // =========================================================================

    println!("🚀 Phase 3: Starting {} Producer Tasks\n", num_producers);

    let start_time = Instant::now();
    let shutdown_flag = Arc::new(AtomicBool::new(false));

    // Spawn metrics display task
    let metrics_clone = Arc::clone(&metrics);
    let shutdown_clone = Arc::clone(&shutdown_flag);
    let metrics_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        while !shutdown_clone.load(Ordering::Relaxed) {
            interval.tick().await;
            print_live_metrics(&metrics_clone);
        }
    });

    // Spawn producer tasks
    let mut producer_handles = Vec::new();

    for producer_id in 0..num_producers {
        let collector_clone = Arc::clone(&collector);
        let verbose = verbose;

        let handle = tokio::spawn(async move {
            run_producer(producer_id, spans_per_producer, collector_clone, verbose).await
        });
        producer_handles.push(handle);
    }

    // Wait for all producers to complete
    let mut producer_results = Vec::new();
    for (id, handle) in producer_handles.into_iter().enumerate() {
        match handle.await {
            Ok(Ok(stats)) => {
                if verbose {
                    println!("   Producer {} completed: {:?}", id, stats);
                }
                producer_results.push(stats);
            }
            Ok(Err(e)) => {
                eprintln!("   ❌ Producer {} failed: {}", id, e);
            }
            Err(e) => {
                eprintln!("   ❌ Producer {} panicked: {}", id, e);
            }
        }
    }

    let generation_time = start_time.elapsed();
    println!("\n   ✅ All producers finished in {:?}", generation_time);

    // Stop metrics display
    shutdown_flag.store(true, Ordering::Relaxed);
    let _ = metrics_task.await;

    // =========================================================================
    // PHASE 4: Graceful Shutdown & Final Drain
    // =========================================================================

    println!("\n🛑 Phase 4: Graceful Shutdown\n");

    // Allow time for final batch processing
    println!("   Waiting for final batch flush...");
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Perform graceful shutdown
    let collector = Arc::try_unwrap(collector)
        .map_err(|_| "Failed to unwrap collector Arc (still referenced)")?;

    collector.shutdown().await?;
    println!("   ✅ Shutdown complete");

    // =========================================================================
    // PHASE 5: Final Statistics & Analysis
    // =========================================================================

    println!("\n📊 Phase 5: Final Statistics\n");

    let total_time = start_time.elapsed();

    // Aggregate producer stats
    let total_submitted: u64 = producer_results.iter().map(|s| s.spans_submitted).sum();
    let total_retries: u64 = producer_results.iter().map(|s| s.backpressure_waits).sum();

    println!("┌─────────────────────────────────────────────────────┐");
    println!("│              SPAN COLLECTOR DEMO RESULTS            │");
    println!("├─────────────────────────────────────────────────────┤");
    println!("│ Total Execution Time:      {:>12.2?}           │", total_time);
    println!("│ Span Generation Time:      {:>12.2?}           │", generation_time);
    println!("├─────────────────────────────────────────────────────┤");
    println!("│ PRODUCER METRICS                                    │");
    println!("│   Producers:               {:>12}             │", num_producers);
    println!("│   Spans per Producer:      {:>12}             │", spans_per_producer);
    println!("│   Total Spans Submitted:   {:>12}             │", total_submitted);
    println!("│   Backpressure Waits:      {:>12}             │", total_retries);
    println!("├─────────────────────────────────────────────────────┤");
    println!("│ COLLECTOR METRICS                                   │");
    println!("│   Spans Submitted:         {:>12}             │", metrics.spans_submitted());
    println!("│   Spans Consumed:          {:>12}             │", metrics.spans_consumed());
    println!("│   Ring Full Events:        {:>12}             │", metrics.full_events());
    println!("│   Reserve Retries:         {:>12}             │", metrics.reserve_retries());
    println!("├─────────────────────────────────────────────────────┤");
    println!("│ THROUGHPUT                                          │");
    let throughput = total_submitted as f64 / total_time.as_secs_f64();
    println!("│   Spans/second:            {:>12.0}             │", throughput);
    let latency_ns = if total_submitted > 0 {
        total_time.as_nanos() as f64 / total_submitted as f64
    } else {
        0.0
    };
    println!("│   Avg Latency:             {:>12.0} ns          │", latency_ns);
    println!("└─────────────────────────────────────────────────────┘");

    // Print backend stats if available
    let (attempts, successes, failures) = backend_stats.stats();
    if attempts > 0 {
        println!("\n📡 Backend Export Statistics:");
        println!("   Export Attempts: {}", attempts);
        println!("   Successful: {} ({:.1}%)", successes, 100.0 * successes as f64 / attempts as f64);
        println!("   Failed: {} ({:.1}%)", failures, 100.0 * failures as f64 / attempts as f64);
    }

    println!("\n✨ Demo completed successfully!\n");

    Ok(())
}

// =============================================================================
// PRODUCER IMPLEMENTATION
// =============================================================================

#[derive(Debug, Clone)]
struct ProducerStats {
    spans_submitted: u64,
    backpressure_waits: u64,
    errors: u64,
}

async fn run_producer(
    producer_id: usize,
    span_count: usize,
    collector: Arc<AsyncSpanCollector>,
    verbose: bool,
) -> Result<ProducerStats, String> {
    // Register this producer with the collector
    let producer = collector
        .register_producer()
        .await
        .map_err(|e| format!("Registration failed: {}", e))?;

    let mut stats = ProducerStats {
        spans_submitted: 0,
        backpressure_waits: 0,
        errors: 0,
    };

    // Simulated service operations
    let operations = [
        ("http.request", SpanKind::Server),
        ("db.query", SpanKind::Client),
        ("cache.get", SpanKind::Client),
        ("grpc.call", SpanKind::Client),
        ("queue.publish", SpanKind::Producer),
        ("process.data", SpanKind::Internal),
        ("validate.input", SpanKind::Internal),
        ("serialize.response", SpanKind::Internal),
    ];

    // Service metadata
    let service_name = format!("service-{}", producer_id % 4);
    let instance_id = format!("instance-{}", producer_id);

    // Rate limiter for this producer (simulating realistic span generation rates)
    let mut rate_limiter = IntervalRateLimiter::from_rate(500.0); // 500 spans/sec max

    for i in 0..span_count {
        // Generate unique trace and span IDs
        let trace_id = generate_trace_id(producer_id, i);
        let span_id = generate_span_id(producer_id, i);
        let parent_span_id = if i > 0 && i % 5 != 0 {
            // Create parent-child relationship (80% have parents)
            generate_span_id(producer_id, i - 1)
        } else {
            0 // Root span
        };

        let (operation, kind) = operations[i % operations.len()].clone();

        // Create span with realistic attributes
        let mut span = Span::new(
            trace_id,
            span_id,
            parent_span_id,
            operation.to_string(),
            kind,
        );

        // Add service-level attributes
        span.set_attribute(
            "service.name".to_string(),
            AttributeValue::String(service_name.clone()),
        );
        span.set_attribute(
            "service.instance.id".to_string(),
            AttributeValue::String(instance_id.clone()),
        );
        span.set_attribute(
            "telemetry.sdk.name".to_string(),
            AttributeValue::String("ringmpsc-otel".to_string()),
        );

        // Add operation-specific attributes
        match operation {
            "http.request" => {
                span.set_attribute(
                    "http.method".to_string(),
                    AttributeValue::String("GET".to_string()),
                );
                span.set_attribute(
                    "http.url".to_string(),
                    AttributeValue::String(format!("/api/v1/resource/{}", i)),
                );
                span.set_attribute(
                    "http.status_code".to_string(),
                    AttributeValue::Int(if i % 10 == 9 { 500 } else { 200 }),
                );
            }
            "db.query" => {
                span.set_attribute(
                    "db.system".to_string(),
                    AttributeValue::String("postgresql".to_string()),
                );
                span.set_attribute(
                    "db.statement".to_string(),
                    AttributeValue::String("SELECT * FROM users WHERE id = ?".to_string()),
                );
                span.set_attribute(
                    "db.rows_affected".to_string(),
                    AttributeValue::Int((i % 100) as i64),
                );
            }
            "cache.get" => {
                span.set_attribute(
                    "cache.hit".to_string(),
                    AttributeValue::Bool(i % 3 != 0),
                );
                span.set_attribute(
                    "cache.key".to_string(),
                    AttributeValue::String(format!("user:{}", i % 1000)),
                );
            }
            "grpc.call" => {
                span.set_attribute(
                    "rpc.system".to_string(),
                    AttributeValue::String("grpc".to_string()),
                );
                span.set_attribute(
                    "rpc.method".to_string(),
                    AttributeValue::String("GetUser".to_string()),
                );
            }
            "queue.publish" => {
                span.set_attribute(
                    "messaging.system".to_string(),
                    AttributeValue::String("kafka".to_string()),
                );
                span.set_attribute(
                    "messaging.destination".to_string(),
                    AttributeValue::String("events-topic".to_string()),
                );
            }
            _ => {}
        }

        // Simulate work duration (affects span timing)
        let work_ms = (i % 20) + 1;
        tokio::time::sleep(Duration::from_millis(work_ms as u64)).await;

        // Finish span with status
        let status = if i % 10 == 9 {
            span.set_attribute("error".to_string(), AttributeValue::Bool(true));
            span.set_attribute(
                "error.message".to_string(),
                AttributeValue::String("Simulated error for testing".to_string()),
            );
            SpanStatus::Error
        } else {
            SpanStatus::Ok
        };
        span.finish(status);

        // Submit span with backpressure handling
        match producer.submit_span(span).await {
            Ok(()) => {
                stats.spans_submitted += 1;
            }
            Err(e) => {
                stats.errors += 1;
                if verbose {
                    eprintln!("Producer {}: submit error: {:?}", producer_id, e);
                }
            }
        }

        // Rate limit span generation
        rate_limiter.wait().await;
    }

    Ok(stats)
}

// =============================================================================
// HELPER FUNCTIONS
// =============================================================================

fn generate_trace_id(producer_id: usize, seq: usize) -> u128 {
    // Create a unique trace ID combining producer, time, and sequence
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;

    ((producer_id as u128) << 112)
        | ((timestamp as u128) << 48)
        | (seq as u128)
}

fn generate_span_id(producer_id: usize, seq: usize) -> u64 {
    ((producer_id as u64) << 48) | (seq as u64 & 0xFFFF_FFFF_FFFF)
}

fn print_banner() {
    println!();
    println!("╔═══════════════════════════════════════════════════════════════════╗");
    println!("║                                                                   ║");
    println!("║     🔭  Distributed Tracing Span Collector Demo                   ║");
    println!("║                                                                   ║");
    println!("║     Built with:                                                   ║");
    println!("║     • ringmpsc-rs   - Lock-free MPSC ring buffer                  ║");
    println!("║     • Rust 2024     - Native async traits                         ║");
    println!("║     • Tokio         - Async runtime                               ║");
    println!("║     • OTel-compatible span data model (not OTLP)                  ║");
    println!("║                                                                   ║");
    println!("╚═══════════════════════════════════════════════════════════════════╝");
    println!();
}

fn print_live_metrics(metrics: &Arc<CollectorMetrics>) {
    print!(
        "\r   📈 Live: submitted={}, consumed={}, full={}, retries={}    ",
        metrics.spans_submitted(),
        metrics.spans_consumed(),
        metrics.full_events(),
        metrics.reserve_retries()
    );
    use std::io::Write;
    std::io::stdout().flush().ok();
}
