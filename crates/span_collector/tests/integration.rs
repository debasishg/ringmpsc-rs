use span_collector::{
    AsyncCollectorConfig, AsyncSpanCollector, Span, SpanKind, SpanStatus,
};
use span_collector::span::SpanBatch;
use span_collector::exporter::{ExportError, SpanExporter};
use std::sync::Arc;
use std::time::Duration;

struct TestExporter {
    spans: std::sync::Mutex<Vec<Span>>,
}

impl TestExporter {
    fn new() -> Self {
        Self {
            spans: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn exported_count(&self) -> usize {
        self.spans.lock().unwrap().len()
    }

    fn spans_by_producer(&self, producer_id: usize) -> Vec<Span> {
        self.spans
            .lock()
            .unwrap()
            .iter()
            .filter(|s| (s.span_id >> 48) as usize == producer_id)
            .cloned()
            .collect()
    }

    fn all_spans(&self) -> Vec<Span> {
        self.spans.lock().unwrap().clone()
    }
}

// Rust 2024: Use native async fn in traits
impl SpanExporter for TestExporter {
    async fn export(&self, batch: SpanBatch) -> Result<(), ExportError> {
        self.spans.lock().unwrap().extend(batch.spans);
        Ok(())
    }

    fn name(&self) -> &str {
        "test"
    }
}

// Slow exporter for backpressure testing
struct SlowExporter {
    delay: Duration,
    spans: std::sync::Mutex<Vec<Span>>,
}

impl SlowExporter {
    fn new(delay: Duration) -> Self {
        Self {
            delay,
            spans: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn exported_count(&self) -> usize {
        self.spans.lock().unwrap().len()
    }
}

// Rust 2024: Use native async fn in traits
impl SpanExporter for SlowExporter {
    async fn export(&self, batch: SpanBatch) -> Result<(), ExportError> {
        tokio::time::sleep(self.delay).await;
        self.spans.lock().unwrap().extend(batch.spans);
        Ok(())
    }

    fn name(&self) -> &str {
        "slow"
    }
}

fn create_test_span(producer_id: usize, seq: u64) -> Span {
    Span::new(
        1, // trace_id
        (producer_id as u64) << 48 | seq,
        0,
        format!("op-{}", seq),
        SpanKind::Internal,
    )
}

#[tokio::test]
async fn test_concurrent_span_submission() {
    let exporter = Arc::new(TestExporter::new());
    let config = AsyncCollectorConfig {
        consumer_interval: Duration::from_millis(50),
        ..Default::default()
    };
    let collector = Arc::new(AsyncSpanCollector::new(config, exporter.clone()).await);

    // 16 producers × 25K spans = 400K total
    let mut tasks = vec![];
    for producer_id in 0..16 {
        let collector_clone = Arc::clone(&collector);
        let task = tokio::spawn(async move {
            let producer = collector_clone.register_producer().await.unwrap();
            for seq in 0..25_000 {
                let span = create_test_span(producer_id, seq);
                producer.submit_span(span).await.unwrap();
            }
        });
        tasks.push(task);
    }

    for task in tasks {
        task.await.unwrap();
    }

    // Wait for processing
    tokio::time::sleep(Duration::from_secs(2)).await;

    let collector = match Arc::try_unwrap(collector) {
        Ok(c) => c,
        Err(_) => panic!("Failed to unwrap Arc"),
    };
    collector.shutdown().await.unwrap();

    // Verify no data loss
    assert_eq!(exporter.exported_count(), 400_000);

    // Verify per-producer FIFO ordering
    for producer_id in 0..16 {
        let producer_spans = exporter.spans_by_producer(producer_id);
        assert_eq!(producer_spans.len(), 25_000);
        
        for window in producer_spans.windows(2) {
            let seq1 = window[0].span_id & 0xFFFF_FFFF_FFFF;
            let seq2 = window[1].span_id & 0xFFFF_FFFF_FFFF;
            assert!(
                seq1 < seq2,
                "Producer {} FIFO violated: {} >= {}",
                producer_id,
                seq1,
                seq2
            );
        }
    }
}

#[tokio::test]
async fn test_backpressure() {
    // Small ring to force backpressure
    let config = AsyncCollectorConfig {
        collector_config: span_collector::CollectorConfig {
            ring_bits: 8, // 256 spans
            max_producers: 16,
            enable_metrics: true,
        },
        consumer_interval: Duration::from_millis(50),
        ..Default::default()
    };

    // Slow exporter that blocks
    let exporter = Arc::new(SlowExporter::new(Duration::from_millis(100)));
    let collector = AsyncSpanCollector::new(config, exporter.clone()).await;

    // Fast producer should block gracefully (not error)
    let producer = collector.register_producer().await.unwrap();
    
    let start = std::time::Instant::now();
    for i in 0..10_000 {
        let span = create_test_span(0, i);
        producer.submit_span(span).await.unwrap();
    }
    let duration = start.elapsed();
    
    println!("Submitted 10K spans with backpressure in {:?}", duration);

    collector.shutdown().await.unwrap();
    
    // All spans eventually exported despite backpressure
    assert_eq!(exporter.exported_count(), 10_000);
}

#[tokio::test]
async fn test_graceful_shutdown_with_inflight_spans() {
    let exporter = Arc::new(TestExporter::new());
    let config = AsyncCollectorConfig::default();
    let collector = AsyncSpanCollector::new(config, exporter.clone()).await;

    let producer = collector.register_producer().await.unwrap();

    // Submit spans
    for i in 0..1000 {
        let span = create_test_span(0, i);
        producer.submit_span(span).await.unwrap();
    }

    // Shutdown should drain all spans
    collector.shutdown().await.unwrap();

    assert_eq!(exporter.exported_count(), 1000);
}

#[tokio::test]
async fn test_mixed_workload() {
    let exporter = Arc::new(TestExporter::new());
    let config = AsyncCollectorConfig {
        consumer_interval: Duration::from_millis(100),
        ..Default::default()
    };
    let collector = Arc::new(AsyncSpanCollector::new(config, exporter.clone()).await);

    // Mix of fast and slow producers
    let mut tasks = vec![];

    // 4 fast producers (1000 spans each)
    for producer_id in 0..4 {
        let collector_clone = Arc::clone(&collector);
        let task = tokio::spawn(async move {
            let producer = collector_clone.register_producer().await.unwrap();
            for seq in 0..1000 {
                let span = create_test_span(producer_id, seq);
                producer.submit_span(span).await.unwrap();
            }
        });
        tasks.push(task);
    }

    // 4 slow producers (100 spans each with delays)
    for producer_id in 4..8 {
        let collector_clone = Arc::clone(&collector);
        let task = tokio::spawn(async move {
            let producer = collector_clone.register_producer().await.unwrap();
            for seq in 0..100 {
                let span = create_test_span(producer_id, seq);
                producer.submit_span(span).await.unwrap();
                tokio::time::sleep(Duration::from_micros(100)).await;
            }
        });
        tasks.push(task);
    }

    for task in tasks {
        task.await.unwrap();
    }

    tokio::time::sleep(Duration::from_millis(500)).await;

    let collector = match Arc::try_unwrap(collector) {
        Ok(c) => c,
        Err(_) => panic!("Failed to unwrap Arc"),
    };
    collector.shutdown().await.unwrap();

    // 4 * 1000 + 4 * 100 = 4400 spans
    assert_eq!(exporter.exported_count(), 4400);
}

#[tokio::test]
async fn test_span_attributes_preserved() {
    let exporter = Arc::new(TestExporter::new());
    let config = AsyncCollectorConfig::default();
    let collector = AsyncSpanCollector::new(config, exporter.clone()).await;

    let producer = collector.register_producer().await.unwrap();

    let mut span = create_test_span(0, 1);
    span.set_attribute(
        "test.key".to_string(),
        span_collector::AttributeValue::String("test.value".to_string()),
    );
    span.finish(SpanStatus::Ok);

    producer.submit_span(span).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;
    collector.shutdown().await.unwrap();

    let spans = exporter.all_spans();
    assert_eq!(spans.len(), 1);
    assert!(spans[0].attributes.contains_key("test.key"));
    assert_eq!(spans[0].status, SpanStatus::Ok);
}
