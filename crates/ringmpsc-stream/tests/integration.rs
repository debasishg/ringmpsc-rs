//! Integration tests for ringmpsc-stream.

use futures::SinkExt;
use ringmpsc_rs::Config;
use ringmpsc_stream::{channel, channel_with_stream_config, StreamConfig, StreamExt};
use std::time::Duration;

#[tokio::test]
async fn test_basic_send_receive() {
    let (factory, mut rx) = channel::<u64>(Config::default());
    let mut tx = factory.register().expect("registration failed");

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
    let mut tx = factory.register().expect("registration failed");

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
    let mut tx1 = factory.register().expect("registration failed");
    let mut tx2 = factory.register().expect("registration failed");

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
    let mut tx = factory.register().expect("registration failed");

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
    let mut tx = factory.register().expect("registration failed");

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
            assert!(item > p, "FIFO violation: {item} came after {p}");
        }
        prev = Some(item);
    }

    assert_eq!(prev, Some(99));
}

// =============================================================================
// Notification race regression tests (INV-STREAM-05)
//
// The register-then-recheck fix catches items pushed between the last ring
// drain and the waker registration (the in-flight race window). These tests
// verify that the fix works correctly.
//
// Architecture note: tokio::sync::Notify's `Notified` future is stack-local
// in poll_next. When poll_next returns Pending, Notified is dropped, which
// de-registers the waker. Any subsequent notify_one() stores a permit but
// cannot wake the task — only the timer can do that. Therefore:
//
//   - The re-drain fixes items arriving DURING poll_next (in-flight race)
//   - The timer catches items arriving AFTER poll_next returns (inter-call)
//
// Test 1 (current_thread, deterministic): Pre-fills the ring with try_send
//   before the receiver runs. With a 60s timer, the only way items can be
//   received is via the permit + re-drain path. This WILL fail without the
//   re-drain fix, proving it works.
//
// Tests 2-3 (multi_thread, practical): Concurrent senders with a realistic
//   timer (100ms / 50ms). Verify correctness (all items, FIFO order) under
//   load. The timer handles inter-call gaps; the re-drain reduces latency
//   for in-flight items.
// =============================================================================

/// Deterministic proof that the re-drain catches items the timer can't reach.
///
/// On `current_thread`, `try_send` pre-fills the ring synchronously. The receiver
/// then runs. With `batch_hint=64` and 100 items, the first drain gets 64 items.
/// The next `poll_next` finds buffer empty, `data_pending=false`, and creates
/// `data_notified` → Pending (Notify is EMPTY, no stored permit). Without the
/// re-drain, the remaining 36 items would be stuck until the 60s timer fires.
/// WITH the re-drain, they're caught immediately.
#[tokio::test(flavor = "current_thread")]
async fn test_recheck_catches_prefilled_ring() {
    let stream_config = StreamConfig::default()
        .with_poll_interval(Duration::from_secs(60))
        .with_batch_hint(64);

    let config = ringmpsc_rs::Config::new(14, 4, false);
    let (factory, mut rx) =
        channel_with_stream_config::<u64>(config, stream_config);
    let mut tx = factory.register().expect("registration failed");

    // Pre-fill 100 items synchronously (all in ring before receiver runs)
    for i in 0..100u64 {
        tx.try_send(i).expect("ring should not be full with 16384 slots");
    }

    // Receive all 100 items. With 60s timer disabled, this MUST use
    // permit consumption + re-drain to find all items.
    let mut received = Vec::new();
    let deadline = tokio::time::timeout(Duration::from_secs(5), async {
        while received.len() < 100 {
            match rx.next().await {
                Some(item) => received.push(item),
                None => break,
            }
        }
    });

    deadline.await.expect(
        "Timed out — re-drain failed to catch items after batch_hint exhausted \
         (remaining items stuck in ring, timer disabled at 60s)",
    );

    assert_eq!(received.len(), 100, "expected all 100 items");
    for i in 0..100u64 {
        assert_eq!(received[i as usize], i, "FIFO violation at index {i}");
    }
}

/// Multi-producer correctness test with realistic timer.
///
/// 4 producers send concurrently on a `multi_thread` runtime. The 100ms timer
/// catches items arriving between `poll_next` calls (when Notified is dropped).
/// The re-drain catches items arriving during `poll_next` (in-flight race).
/// Together they ensure all items are received with per-producer FIFO order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_no_lost_wakeup_multi_producer() {
    let stream_config = StreamConfig::default()
        .with_poll_interval(Duration::from_millis(100))
        .with_batch_hint(64);

    let (factory, mut rx) =
        channel_with_stream_config::<u64>(Config::default(), stream_config);

    let num_producers = 4;
    let items_per_producer = 100u64;
    let total_items = num_producers * items_per_producer as usize;

    let mut handles = Vec::new();
    for p in 0..num_producers {
        let mut tx = factory.register().expect("registration failed");
        handles.push(tokio::spawn(async move {
            for i in 0..items_per_producer {
                let val = (p as u64) * 1000 + i;
                tx.send(val).await.expect("send failed");
                if i % 5 == 0 {
                    tokio::task::yield_now().await;
                }
            }
        }));
    }

    let mut received = Vec::new();
    let deadline = tokio::time::timeout(Duration::from_secs(10), async {
        while received.len() < total_items {
            match rx.next().await {
                Some(item) => received.push(item),
                None => break,
            }
        }
    });

    deadline.await.expect(
        "Timed out waiting for items (100ms timer should catch inter-call gaps)",
    );

    for h in handles {
        h.await.expect("sender panicked");
    }

    assert_eq!(received.len(), total_items, "expected all {total_items} items");

    // Verify per-producer FIFO ordering
    for p in 0..num_producers {
        let base = (p as u64) * 1000;
        let producer_items: Vec<u64> = received
            .iter()
            .copied()
            .filter(|&v| v >= base && v < base + items_per_producer)
            .collect();
        assert_eq!(
            producer_items.len(),
            items_per_producer as usize,
            "producer {p}: missing items"
        );
        for (idx, &val) in producer_items.iter().enumerate() {
            assert_eq!(
                val,
                base + idx as u64,
                "producer {p}: FIFO violation at index {idx}"
            );
        }
    }
}

/// Burst-pattern test: rapid sends with pauses between bursts.
///
/// The 5ms pauses create windows where the receiver returns Pending with no
/// active senders. The 50ms timer catches these inter-burst gaps. Within
/// each burst, the re-drain catches items pushed during `poll_next` execution.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_no_lost_wakeup_burst_pattern() {
    let stream_config = StreamConfig::default()
        .with_poll_interval(Duration::from_millis(50))
        .with_batch_hint(16);

    let (factory, mut rx) =
        channel_with_stream_config::<u64>(Config::default(), stream_config);
    let mut tx = factory.register().expect("registration failed");

    let total = 500u64;
    let sender = tokio::spawn(async move {
        for burst in 0..10u64 {
            for i in 0..50u64 {
                tx.send(burst * 50 + i)
                    .await
                    .expect("send failed");
            }
            // Pause between bursts — receiver returns Pending, Notified dropped,
            // timer must re-wake the task for the next burst's items
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });

    let mut received = Vec::new();
    let deadline = tokio::time::timeout(Duration::from_secs(10), async {
        while received.len() < total as usize {
            match rx.next().await {
                Some(item) => received.push(item),
                None => break,
            }
        }
    });

    deadline.await.expect(
        "Timed out — burst pattern with 50ms timer should complete well within 10s",
    );

    sender.await.expect("sender panicked");
    assert_eq!(received.len(), total as usize);
}

/// #45 follow-up: a sender waiting for space on a full ring must return
/// `Closed` when the factory closes, not wait forever for a drain.
#[tokio::test]
async fn test_waiting_sender_returns_closed_on_factory_close() {
    // 2-slot ring, and the receiver is never polled, so nothing drains it.
    let (factory, _rx) = channel::<u64>(Config::new(1, 1, false));
    let mut tx = factory.register().expect("one sender");
    let mut filled = 0;
    while tx.try_send(filled).is_ok() {
        filled += 1;
    }
    assert!(filled > 0, "the ring took at least one item");

    let waiting = tokio::spawn(async move { tx.send(99).await });
    // Let the sender reach its wait.
    tokio::time::sleep(Duration::from_millis(50)).await;
    factory.close();

    let result = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .expect("send must not hang after close")
        .expect("sender task");
    assert!(
        matches!(result, Err(ringmpsc_stream::StreamError::Closed)),
        "got {result:?}"
    );
}

/// A send after the factory closes fails at once.
#[tokio::test]
async fn test_send_after_factory_close_is_closed() {
    let (factory, _rx) = channel::<u64>(Config::new(3, 1, false));
    let mut tx = factory.register().expect("one sender");
    factory.close();
    let result = tokio::time::timeout(Duration::from_secs(2), tx.send(1))
        .await
        .expect("send must not hang after close");
    assert!(matches!(result, Err(ringmpsc_stream::StreamError::Closed)));
}

/// Fills a 2-slot ring and returns how many items it took.
fn fill(tx: &mut ringmpsc_stream::RingSender<u64>) -> u64 {
    let mut n = 0;
    while tx.try_send(n).is_ok() {
        n += 1;
    }
    assert!(n > 0, "the ring took at least one item");
    n
}

/// #47: a `SinkExt::send` waiting on a full ring completes once the receiver
/// drains. It used to drop its `Notified` on `Pending`, so nothing woke it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_sink_send_wakes_after_drain() {
    let (factory, mut rx) = channel::<u64>(Config::new(1, 1, false));
    let mut tx = factory.register().expect("one sender");
    let n = fill(&mut tx);

    let sender = tokio::spawn(async move { SinkExt::send(&mut tx, 99).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    for _ in 0..n {
        rx.next().await.expect("a filled item");
    }

    tokio::time::timeout(Duration::from_secs(2), sender)
        .await
        .expect("SinkExt::send must complete after the drain")
        .expect("sender task")
        .expect("send succeeds");
    assert_eq!(rx.next().await, Some(99));
}

/// #47, `feed` + `flush`: the pending item goes through `poll_flush`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_sink_flush_wakes_after_drain() {
    let (factory, mut rx) = channel::<u64>(Config::new(1, 1, false));
    let mut tx = factory.register().expect("one sender");
    let n = fill(&mut tx);

    let sender = tokio::spawn(async move {
        tx.feed(99).await?;
        tx.flush().await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    for _ in 0..n {
        rx.next().await.expect("a filled item");
    }

    tokio::time::timeout(Duration::from_secs(2), sender)
        .await
        .expect("flush must complete after the drain")
        .expect("sender task")
        .expect("flush succeeds");
    assert_eq!(rx.next().await, Some(99));
}

/// A `Sink` sender waiting on a full ring returns `Closed` when the factory
/// closes, instead of waiting forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_waiting_sink_returns_closed_on_factory_close() {
    let (factory, _rx) = channel::<u64>(Config::new(1, 1, false));
    let mut tx = factory.register().expect("one sender");
    fill(&mut tx);

    let sender = tokio::spawn(async move { SinkExt::send(&mut tx, 99).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    factory.close();

    let result = tokio::time::timeout(Duration::from_secs(2), sender)
        .await
        .expect("SinkExt::send must not hang after close")
        .expect("sender task");
    assert!(
        matches!(result, Err(ringmpsc_stream::StreamError::Closed)),
        "got {result:?}"
    );
}

/// INV-SHUT-04: every send that returns `Ok` is received, even when it races
/// the close. Senders push as fast as they can while another task closes the
/// factory; the receiver must yield exactly as many items as succeeded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_every_ok_send_is_received_across_close() {
    const SENDERS: usize = 4;
    for round in 0..50u64 {
        let (factory, rx) = channel::<u64>(Config::new(4, SENDERS, false));
        let senders: Vec<_> = (0..SENDERS)
            .map(|_| {
                let mut tx = factory.register().expect("sender");
                tokio::spawn(async move {
                    let mut ok = 0u64;
                    while tx.send(ok).await.is_ok() {
                        ok += 1;
                    }
                    ok
                })
            })
            .collect();
        let mut rx = rx;
        let receiver = tokio::spawn(async move {
            let mut n = 0u64;
            while rx.next().await.is_some() {
                n += 1;
            }
            n
        });

        // Close after a varying delay, so it lands in different places.
        tokio::time::sleep(Duration::from_micros(200 + 37 * round)).await;
        factory.close();

        let mut sent = 0;
        for s in senders {
            sent += tokio::time::timeout(Duration::from_secs(5), s)
                .await
                .expect("senders stop after close")
                .expect("sender task");
        }
        let received = tokio::time::timeout(Duration::from_secs(5), receiver)
            .await
            .expect("receiver ends after close")
            .expect("receiver task");
        assert_eq!(received, sent, "round {round}: an Ok send was not received");
    }
}

/// An item `feed` accepted but couldn't commit before a close isn't lost
/// silently: `flush` reports `Closed`, and `take_pending` returns it.
#[tokio::test]
async fn test_pending_item_is_recoverable_after_close() {
    let (factory, _rx) = channel::<u64>(Config::new(1, 1, false));
    let mut tx = factory.register().expect("one sender");
    fill(&mut tx);

    // Ring full: `start_send` parks the item and `feed` returns Ok.
    let parked = futures::future::poll_fn(|cx| {
        use futures::Sink;
        let mut pinned = std::pin::Pin::new(&mut tx);
        match pinned.as_mut().poll_ready(cx) {
            std::task::Poll::Ready(Ok(())) => std::task::Poll::Ready(pinned.start_send(99)),
            other => other,
        }
    })
    .await;
    assert!(parked.is_ok(), "accepted while the ring is full");

    factory.close();
    let flushed = tokio::time::timeout(Duration::from_secs(2), tx.flush())
        .await
        .expect("flush must not hang after close");
    assert!(matches!(flushed, Err(ringmpsc_stream::StreamError::Closed)));
    assert_eq!(tx.take_pending(), Some(99), "the unsent item is handed back");
}

/// An idle receiver ends the stream promptly after `ShutdownSignal::shutdown()`
/// from another task, without waiting for its poll tick.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_shutdown_signal_wakes_idle_receiver() {
    let config = StreamConfig {
        poll_interval: Duration::from_secs(30),
        ..StreamConfig::default()
    };
    let (_factory, mut rx) = channel_with_stream_config::<u64>(Config::new(3, 1, false), config);
    let signal = rx.shutdown_signal();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        signal.shutdown();
    });
    // Measure, don't just bound: `timeout` re-polls the inner future when
    // its deadline fires, which would itself wake the receiver.
    let start = std::time::Instant::now();
    let ended = tokio::time::timeout(Duration::from_secs(5), rx.next())
        .await
        .expect("receiver must notice the shutdown");
    assert_eq!(ended, None);
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "receiver noticed the shutdown only after {:?}",
        start.elapsed()
    );
}

/// An idle receiver wakes for new data on the sender's notify, not only on
/// its poll tick: the wait on `data_notify` is kept across polls.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_idle_receiver_wakes_for_data_without_tick() {
    let config = StreamConfig {
        poll_interval: Duration::from_secs(30),
        ..StreamConfig::default()
    };
    let (factory, mut rx) = channel_with_stream_config::<u64>(Config::new(3, 1, false), config);
    let mut tx = factory.register().expect("one sender");
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(5).await.expect("send");
        tx
    });
    // Measure, don't just bound: `timeout` re-polls the inner future when
    // its deadline fires, which would itself wake the receiver.
    let start = std::time::Instant::now();
    let got = tokio::time::timeout(Duration::from_secs(5), rx.next())
        .await
        .expect("receiver must wake for data");
    assert_eq!(got, Some(5));
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "receiver woke for data only after {:?}",
        start.elapsed()
    );
}
