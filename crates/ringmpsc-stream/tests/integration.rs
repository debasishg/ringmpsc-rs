//! Integration tests for ringmpsc-stream.

use futures::SinkExt;
use ringmpsc_rs::Config;
use ringmpsc_stream::{channel, StreamConfig, StreamExt};
use std::time::Duration;

#[tokio::test]
async fn test_basic_send_receive() {
    let (factory, mut rx) = channel::<u64>(Config::default());
    let tx = factory.register().expect("registration failed");

    // Send items
    tx.send(1).await.expect("send failed");
    tx.send(2).await.expect("send failed");
    tx.send(3).await.expect("send failed");

    // Close sender to signal no more items
    tx.close();
    factory.close();

    // Receive items
    let mut received = Vec::new();
    while let Some(item) = rx.next().await {
        received.push(item);
    }

    assert_eq!(received, vec![1, 2, 3]);
}

#[tokio::test]
async fn test_try_send_preserves_item_on_full() {
    // Create a small ring buffer
    let config = Config::new(2, 1, false); // 4 slots, 1 producer, no metrics
    let (factory, _rx) = channel::<u64>(config);
    let tx = factory.register().expect("registration failed");

    // Fill the ring
    for i in 0..4 {
        tx.try_send(i).expect("should succeed");
    }

    // Next send should fail and return the item
    let result = tx.try_send(100);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), 100); // Item preserved!
}

#[tokio::test]
async fn test_multiple_producers() {
    let (factory, mut rx) = channel::<u64>(Config::default());

    // Register multiple senders
    let tx1 = factory.register().expect("registration failed");
    let tx2 = factory.register().expect("registration failed");

    // Send from both producers
    tx1.send(10).await.expect("send failed");
    tx2.send(20).await.expect("send failed");
    tx1.send(11).await.expect("send failed");
    tx2.send(21).await.expect("send failed");

    // Close senders
    tx1.close();
    tx2.close();
    factory.close();

    // Receive all items
    let mut received = Vec::new();
    while let Some(item) = rx.next().await {
        received.push(item);
    }

    // Should have all 4 items (order may vary between producers)
    assert_eq!(received.len(), 4);
    assert!(received.contains(&10));
    assert!(received.contains(&11));
    assert!(received.contains(&20));
    assert!(received.contains(&21));
}

#[tokio::test]
async fn test_sink_trait() {
    let (factory, mut rx) = channel::<u64>(Config::default());
    let mut tx = factory.register().expect("registration failed");

    // Use Sink trait methods
    tx.send(42).await.expect("send failed");
    tx.flush().await.expect("flush failed");
    tx.close(); // close() is synchronous

    factory.close();

    // Receive
    let item = rx.next().await;
    assert_eq!(item, Some(42));
}

#[tokio::test]
async fn test_graceful_shutdown() {
    let (factory, mut rx) = channel::<u64>(Config::default());
    let tx = factory.register().expect("registration failed");

    // Send some items
    tx.send(1).await.expect("send failed");
    tx.send(2).await.expect("send failed");

    // Trigger shutdown
    rx.shutdown();

    // Should still receive items
    let mut received = Vec::new();
    while let Some(item) = rx.next().await {
        received.push(item);
    }

    assert_eq!(received, vec![1, 2]);
}

#[tokio::test]
async fn test_stream_config() {
    let config = StreamConfig::low_latency();
    assert_eq!(config.poll_interval, Duration::from_millis(1));
    assert_eq!(config.batch_hint, 16);

    let config = StreamConfig::high_throughput();
    assert_eq!(config.poll_interval, Duration::from_millis(50));
    assert_eq!(config.batch_hint, 256);

    let config = StreamConfig::default()
        .with_poll_interval(Duration::from_millis(5))
        .with_batch_hint(128);
    assert_eq!(config.poll_interval, Duration::from_millis(5));
    assert_eq!(config.batch_hint, 128);
}

#[tokio::test]
async fn test_closed_channel_error() {
    let (factory, _rx) = channel::<u64>(Config::default());
    let tx = factory.register().expect("registration failed");

    // Close the factory
    factory.close();

    // New registrations should fail
    let result = factory.register();
    assert!(result.is_err());

    // Existing sender should detect closure eventually
    // (this depends on implementation details)
    tx.close();
}

#[tokio::test]
async fn test_fifo_ordering_single_producer() {
    let (factory, mut rx) = channel::<u64>(Config::default());
    let tx = factory.register().expect("registration failed");

    // Send items in order
    for i in 0..100 {
        tx.send(i).await.expect("send failed");
    }

    tx.close();
    factory.close();

    // Receive and verify order
    let mut prev = None;
    while let Some(item) = rx.next().await {
        if let Some(p) = prev {
            assert!(item > p, "FIFO violation: {} came after {}", item, p);
        }
        prev = Some(item);
    }

    assert_eq!(prev, Some(99));
}
