# ringmpsc-stream

Async `Stream`/`Sink` adapters for [ringmpsc-rs](../ringmpsc) with backpressure support.

## Features

- **`futures::Stream`** implementation for async receiving
- **`futures::Sink`** implementation for async sending
- **Hybrid polling**: Event-driven via `Notify` + configurable poll interval as safety net
- **Backpressure**: Senders block when ring is full, woken when space available
- **Graceful shutdown**: Drains remaining items before terminating
- **Zero-copy path**: Inherits ringmpsc's ownership transfer semantics

## Quick Start

```rust
use ringmpsc_stream::{channel, StreamExt};
use ringmpsc_rs::Config;

#[tokio::main]
async fn main() {
    // Create channel with default config
    let (factory, mut rx) = channel::<u64>(Config::default());

    // Register senders explicitly (each gets its own ring)
    let tx = factory.register().expect("registration failed");

    // Spawn sender task
    tokio::spawn(async move {
        for i in 0..100 {
            tx.send(i).await.expect("send failed");
        }
        tx.close();
    });

    // Receive as async stream
    while let Some(item) = rx.next().await {
        println!("Received: {}", item);
    }
}
```

## Configuration

```rust
use ringmpsc_stream::{channel_with_stream_config, StreamConfig};
use ringmpsc_rs::Config;

// Low latency (1ms poll, batch 16)
let (factory, rx) = channel_with_stream_config::<u64>(
    Config::default(),
    StreamConfig::low_latency(),
);

// High throughput (50ms poll, batch 256)
let (factory, rx) = channel_with_stream_config::<u64>(
    Config::default(),
    StreamConfig::high_throughput(),
);

// Custom
let (factory, rx) = channel_with_stream_config::<u64>(
    Config::default(),
    StreamConfig::default()
        .with_poll_interval(std::time::Duration::from_millis(5))
        .with_batch_hint(128),
);
```

## Multi-Producer Pattern

```rust
use ringmpsc_stream::channel;
use ringmpsc_rs::Config;

let (factory, mut rx) = channel::<u64>(Config::default());

// Each thread/task registers its own sender
let tx1 = factory.register().expect("registration failed");
let tx2 = factory.register().expect("registration failed");

// Senders do NOT implement Clone - this is intentional!
// Each sender has its own dedicated ring buffer.
```

## Graceful Shutdown

```rust
// Internal shutdown (drains remaining items)
rx.shutdown();
while let Some(item) = rx.next().await {
    // Process remaining items
}

// Or compose with external cancellation
use tokio_util::sync::CancellationToken;
use tokio_stream::StreamExt;

let token = CancellationToken::new();
let stream = rx.take_until(token.cancelled());
```

## Design

See [spec.md](spec.md) for invariants and design rationale.

Key patterns borrowed from [span_collector](../span_collector):
- Hybrid polling (event-driven + interval safety net)
- `Notify`-based backpressure signaling
- Graceful shutdown via oneshot channel

## License

MIT
