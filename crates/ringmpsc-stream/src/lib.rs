//! Async Stream/Sink Adapters for ringmpsc-rs
//!
//! This crate provides [`futures::Stream`] and [`futures::Sink`] implementations
//! for ringmpsc channels, enabling async pipeline processing with backpressure.
//!
//! # Features
//!
//! - **Hybrid polling**: Event-driven via `Notify` + configurable poll interval as safety net
//! - **Backpressure**: Senders await when ring is full, woken when space available
//! - **Graceful shutdown**: Internal oneshot + composable with `take_until` for flexibility
//! - **Zero-copy path**: Inherits ringmpsc's ownership transfer semantics
//!
//! # Example
//!
//! ```ignore
//! use ringmpsc_stream::channel;
//! use ringmpsc_rs::Config;
//! use tokio_stream::StreamExt;
//! use futures_sink::SinkExt;
//!
//! #[tokio::main]
//! async fn main() {
//!     let (factory, mut rx) = channel::<u64>(Config::default());
//!     
//!     // Register a sender (explicit, not Clone)
//!     let mut tx = factory.register().unwrap();
//!     
//!     // Send items
//!     tx.send(42).await.unwrap();
//!     tx.send(43).await.unwrap();
//!     
//!     // Receive as async stream
//!     while let Some(item) = rx.next().await {
//!         println!("Received: {}", item);
//!     }
//! }
//! ```

mod channel;
mod config;
mod error;
mod invariants;
mod receiver;
mod sender;
mod shutdown;

pub use channel::{channel, channel_with_stream_config, SenderFactory};
pub use config::StreamConfig;
pub use error::StreamError;
pub use receiver::RingReceiver;
pub use sender::RingSender;
pub use shutdown::ShutdownSignal;

// Re-export useful stream combinators
pub use tokio_stream::StreamExt;
