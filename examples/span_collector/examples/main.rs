use span_collector::{
    AsyncCollectorConfig, AsyncSpanCollector, AttributeValue, Span, SpanKind, SpanStatus,
    StdoutExporter,
};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== OpenTelemetry Span Collector Demo ===\n");

    // Create collector with stdout exporter
    let exporter = Arc::new(StdoutExporter::new(true)); // verbose mode
    let config = AsyncCollectorConfig {
        consumer_interval: Duration::from_millis(100),
        ..Default::default()
    };

    println!("Configuration:");
    println!("  Ring buffer size: {} spans", 1 << config.collector_config.ring_bits);
    println!("  Max producers: {}", config.collector_config.max_producers);
    println!("  Batch size limit: {}", config.batch_config.batch_size_limit);
    println!("  Consumer interval: {:?}\n", config.consumer_interval);

    let collector = Arc::new(AsyncSpanCollector::new(config, exporter).await);

    // Spawn 8 producer tasks (simulating microservice threads)
    println!("Starting 8 producer tasks...\n");
    let mut tasks = vec![];
    
    for producer_id in 0..8 {
        let collector_clone = Arc::clone(&collector);
        let task = tokio::spawn(async move {
            generate_spans(producer_id, collector_clone).await
        });
        tasks.push(task);
    }

    // Wait for producers to finish
    for (i, task) in tasks.into_iter().enumerate() {
        task.await??;
        println!("Producer {} finished", i);
    }

    println!("\nAll producers finished. Shutting down...");

    // Wait a bit for final processing
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Graceful shutdown
    let collector = Arc::try_unwrap(collector)
        .map_err(|_| "Failed to unwrap Arc")?;
    collector.shutdown().await?;

    // Print final metrics
    println!("\n=== Final Metrics ===");
    // Note: metrics are already dropped with collector
    println!("Shutdown complete!");

    Ok(())
}

async fn generate_spans(
    producer_id: usize,
    collector: Arc<AsyncSpanCollector>,
) -> Result<(), String> {
    let producer = collector.register_producer().await
        .map_err(|e| e.to_string())?;

    let operations = vec![
        "http.request",
        "db.query",
        "cache.get",
        "api.call",
        "process.data",
    ];

    // Generate 50 spans per producer
    for i in 0..50 {
        let trace_id = (producer_id as u128) << 64 | (i as u128);
        let span_id = (producer_id as u64) << 48 | i;
        let operation = operations[i as usize % operations.len()];

        let mut span = Span::new(
            trace_id,
            span_id,
            0,
            operation.to_string(),
            SpanKind::Server,
        );

        // Add some attributes
        span.set_attribute("producer.id".to_string(), AttributeValue::Int(producer_id as i64));
        span.set_attribute("span.sequence".to_string(), AttributeValue::Int(i as i64));
        span.set_attribute("service.name".to_string(), AttributeValue::String("demo-service".to_string()));

        // Simulate some work
        let work_duration = (i % 10) + 1;
        tokio::time::sleep(Duration::from_millis(work_duration)).await;

        // Finish span
        let status = if (i % 10) != 9 {
            SpanStatus::Ok
        } else {
            span.set_attribute("error".to_string(), AttributeValue::Bool(true));
            SpanStatus::Error
        };
        span.finish(status);

        // Submit span
        producer.submit_span(span).await
            .map_err(|e| e.to_string())?;

        // Varying rates - occasional burst
        if i % 10 == 0 {
            tokio::time::sleep(Duration::from_micros(100)).await;
        }
    }

    Ok(())
}
