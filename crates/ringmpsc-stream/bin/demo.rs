//! Demonstration of ringmpsc-stream features.
//!
//! Run with: `cargo run -p ringmpsc-stream --bin demo`

use futures_sink::Sink;
use ringmpsc_rs::Config;
use ringmpsc_stream::{
    channel, channel_with_stream_config, RingSender, ShutdownSignal, StreamConfig, StreamExt,
};
use std::pin::Pin;
use std::time::Duration;
use tokio::time::timeout;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== ringmpsc-stream Demo ===\n");

    demo_basic_usage().await?;
    demo_multiple_producers().await?;
    demo_backpressure().await?;
    demo_sink_trait().await?;
    demo_configuration_presets().await?;
    demo_graceful_shutdown().await?;

    println!("\n=== All demos completed successfully! ===");
    Ok(())
}

/// Demo 1: Basic channel creation and send/receive
async fn demo_basic_usage() -> Result<(), Box<dyn std::error::Error>> {
    println!("--- Demo 1: Basic Usage ---");

    // Create a channel with default configuration
    // Config::new(ring_bits, max_producers, enable_metrics)
    let config = Config::new(8, 4, false); // 256-slot rings, 4 max producers
    let (factory, mut rx) = channel::<u64>(config);

    // Explicitly register a sender (no Clone - each sender is unique)
    let tx = factory.register()?;

    // Spawn producer task
    let producer = tokio::spawn(async move {
        for i in 0..5 {
            tx.send(i).await.expect("send failed");
            println!("  Sent: {}", i);
        }
        // Sender dropped here - no more items from this producer
    });

    // Consume items via Stream
    let mut count = 0;
    while let Ok(Some(item)) = timeout(Duration::from_millis(100), rx.next()).await {
        println!("  Received: {}", item);
        count += 1;
        if count >= 5 {
            break;
        }
    }

    producer.await?;
    println!("  ✓ Basic usage complete\n");
    Ok(())
}

/// Demo 2: Multiple producers with explicit registration
async fn demo_multiple_producers() -> Result<(), Box<dyn std::error::Error>> {
    println!("--- Demo 2: Multiple Producers ---");

    let config = Config::new(8, 4, false);
    let (factory, mut rx) = channel::<String>(config);

    // Register multiple senders - each gets its own ring buffer
    let tx1 = factory.register()?;
    let tx2 = factory.register()?;
    let tx3 = factory.register()?;

    println!("  Registered 3 producers");

    // Each producer sends from its own thread
    let p1 = tokio::spawn(async move {
        for i in 0..3 {
            tx1.send(format!("P1-{}", i)).await.ok();
        }
    });

    let p2 = tokio::spawn(async move {
        for i in 0..3 {
            tx2.send(format!("P2-{}", i)).await.ok();
        }
    });

    let p3 = tokio::spawn(async move {
        for i in 0..3 {
            tx3.send(format!("P3-{}", i)).await.ok();
        }
    });

    // Wait for producers
    let _ = tokio::join!(p1, p2, p3);

    // Drain all items
    let mut received = Vec::new();
    while let Ok(Some(item)) = timeout(Duration::from_millis(100), rx.next()).await {
        received.push(item);
    }

    println!("  Received {} items: {:?}", received.len(), received);

    // Verify FIFO per-producer (items from same producer maintain order)
    let p1_items: Vec<_> = received.iter().filter(|s| s.starts_with("P1")).collect();
    let p2_items: Vec<_> = received.iter().filter(|s| s.starts_with("P2")).collect();
    let p3_items: Vec<_> = received.iter().filter(|s| s.starts_with("P3")).collect();

    println!("  P1 order: {:?}", p1_items);
    println!("  P2 order: {:?}", p2_items);
    println!("  P3 order: {:?}", p3_items);
    println!("  ✓ Multiple producers complete\n");
    Ok(())
}

/// Demo 3: Backpressure handling with try_send
async fn demo_backpressure() -> Result<(), Box<dyn std::error::Error>> {
    println!("--- Demo 3: Backpressure Handling ---");

    // Small ring to demonstrate backpressure
    let config = Config::new(4, 2, false); // 16-slot ring
    let (factory, mut rx) = channel::<u64>(config);
    let tx = factory.register()?;

    // Fill the ring without consuming
    let mut sent = 0;
    let mut full_count = 0;

    for i in 0..32 {
        // try_send returns Result<(), T> - the item on failure
        match tx.try_send(i) {
            Ok(()) => {
                sent += 1;
            }
            Err(_rejected_item) => {
                // Ring was full, item was NOT consumed
                full_count += 1;
            }
        }
    }

    println!("  Sent {} items, {} were rejected (Full)", sent, full_count);

    // Drain to relieve backpressure
    let mut drained = 0;
    while let Ok(Some(_)) = timeout(Duration::from_millis(50), rx.next()).await {
        drained += 1;
    }

    println!("  Drained {} items", drained);

    // Now async send() handles backpressure automatically
    let tx2 = factory.register()?;
    println!("  Using async send() with automatic backpressure...");

    let producer = tokio::spawn(async move {
        for i in 0..20 {
            // send() waits for space if ring is full
            tx2.send(i).await.ok();
        }
    });

    // Slow consumer
    let consumer = tokio::spawn(async move {
        let mut count = 0;
        while let Ok(Some(_)) = timeout(Duration::from_millis(200), rx.next()).await {
            count += 1;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        count
    });

    producer.await?;
    let consumed = consumer.await?;
    println!("  Consumer received {} items with backpressure", consumed);
    println!("  ✓ Backpressure handling complete\n");
    Ok(())
}

/// Demo 4: Using the Sink trait
async fn demo_sink_trait() -> Result<(), Box<dyn std::error::Error>> {
    println!("--- Demo 4: Sink Trait ---");

    let config = Config::new(8, 2, false);
    let (factory, mut rx) = channel::<i32>(config);
    let mut tx = factory.register()?;

    // Use Sink trait methods
    let mut pinned: Pin<&mut RingSender<i32>> = Pin::new(&mut tx);

    // Direct Sink usage via poll functions
    let waker = futures_util::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);

    // Check ready
    match pinned.as_mut().poll_ready(&mut cx) {
        std::task::Poll::Ready(Ok(())) => println!("  Sink is ready"),
        std::task::Poll::Ready(Err(e)) => println!("  Sink error: {:?}", e),
        std::task::Poll::Pending => println!("  Sink pending (would wait for space)"),
    }

    // Send an item via start_send
    pinned.as_mut().start_send(42)?;
    println!("  Sent 42 via Sink::start_send");

    // Flush to ensure delivery
    match pinned.as_mut().poll_flush(&mut cx) {
        std::task::Poll::Ready(Ok(())) => println!("  Sink flushed"),
        _ => println!("  Flush pending"),
    }

    // Receive
    if let Ok(Some(item)) = timeout(Duration::from_millis(100), rx.next()).await {
        println!("  Received via Stream: {}", item);
    }

    // Close the sink
    match pinned.as_mut().poll_close(&mut cx) {
        std::task::Poll::Ready(Ok(())) => println!("  Sink closed"),
        _ => println!("  Close pending"),
    }

    println!("  ✓ Sink trait demo complete\n");
    Ok(())
}

/// Demo 5: Configuration presets
async fn demo_configuration_presets() -> Result<(), Box<dyn std::error::Error>> {
    println!("--- Demo 5: Configuration Presets ---");

    let ring_config = Config::new(8, 2, false);

    // Low-latency configuration (1ms poll, batch 16)
    let low_latency = StreamConfig::low_latency();
    println!(
        "  Low-latency: poll_interval={:?}, batch_hint={}",
        low_latency.poll_interval, low_latency.batch_hint
    );

    let (factory, mut rx) = channel_with_stream_config::<u64>(ring_config, low_latency);
    let tx = factory.register()?;
    tx.send(1).await?;

    if let Ok(Some(v)) = timeout(Duration::from_millis(50), rx.next()).await {
        println!("  Received {} with low-latency config", v);
    }

    // High-throughput configuration (50ms poll, batch 256)
    let ring_config = Config::new(8, 2, false);
    let high_throughput = StreamConfig::high_throughput();
    println!(
        "  High-throughput: poll_interval={:?}, batch_hint={}",
        high_throughput.poll_interval, high_throughput.batch_hint
    );

    let (factory, mut rx) = channel_with_stream_config::<u64>(ring_config, high_throughput);
    let tx = factory.register()?;

    // Send a batch
    for i in 0..10 {
        tx.send(i).await?;
    }

    let mut count = 0;
    while let Ok(Some(_)) = timeout(Duration::from_millis(100), rx.next()).await {
        count += 1;
    }
    println!("  Received {} items with high-throughput config", count);

    // Custom configuration
    let custom = StreamConfig {
        poll_interval: Duration::from_millis(25),
        batch_hint: 128,
    };
    println!(
        "  Custom: poll_interval={:?}, batch_hint={}",
        custom.poll_interval, custom.batch_hint
    );

    println!("  ✓ Configuration presets complete\n");
    Ok(())
}

/// Demo 6: Graceful shutdown with ShutdownSignal
async fn demo_graceful_shutdown() -> Result<(), Box<dyn std::error::Error>> {
    println!("--- Demo 6: Graceful Shutdown ---");

    let config = Config::new(8, 2, false);
    let (factory, rx) = channel::<u64>(config);
    let tx = factory.register()?;

    // Get a cloneable shutdown signal
    let shutdown_signal: ShutdownSignal = rx.shutdown_signal();

    // Spawn producer that sends continuously
    let signal_for_producer = shutdown_signal.clone();
    let producer = tokio::spawn(async move {
        let mut sent = 0u64;
        loop {
            // Check if shutdown was requested
            if signal_for_producer.is_shutdown() {
                println!("  Producer observed shutdown after {} sends", sent);
                break;
            }

            match tx.try_send(sent) {
                Ok(()) => sent += 1,
                Err(_) => {
                    // Backpressure - wait a bit
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
        }
        sent
    });

    // Spawn consumer
    let consumer = tokio::spawn(async move {
        let mut rx = rx;
        let mut received = 0u64;
        while let Some(_item) = rx.next().await {
            received += 1;
            if received % 100 == 0 {
                tokio::task::yield_now().await;
            }
        }
        println!("  Consumer received {} items before Stream ended", received);
        received
    });

    // Let it run for a bit
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Trigger graceful shutdown via the signal
    println!("  Triggering shutdown via ShutdownSignal...");
    shutdown_signal.shutdown();

    // Wait for tasks
    let (sent, received) = tokio::join!(producer, consumer);
    let sent = sent?;
    let received = received?;

    println!("  Final: sent={}, received={}", sent, received);
    println!("  ✓ Graceful shutdown complete (all in-flight items drained)\n");
    Ok(())
}
