//! RingMPSC - Lock-Free Multi-Producer Single-Consumer Channel
//!
//! A ring-decomposed MPSC implementation where each producer has a dedicated
//! SPSC ring buffer. This eliminates producer-producer contention entirely.
//!
//! This is a Rust port of the [RingMPSC Zig implementation](https://github.com/boonzy00/ringmpsc).
//!
//! # Key Features
//!
//! - 128-byte alignment (prefetcher false sharing elimination)
//! - Batch consumption API (single head update for N items)
//! - Adaptive backoff (spin → yield → park)
//! - Zero-copy reserve/commit API
//!
//! Achieves 50+ billion messages/second on AMD Ryzen 7 5700.
//!
//! # Example
//!
//! ```
//! use ringmpsc_rs::{Channel, Config};
//! use std::mem::MaybeUninit;
//!
//! let channel = Channel::<u64>::new(Config::default());
//! let producer = channel.register().unwrap();
//!
//! // Simple API: push() for single items
//! producer.push(42);
//!
//! // Zero-copy API: reserve() + MaybeUninit for maximum performance
//! if let Some(mut reservation) = producer.reserve(1) {
//!     reservation.as_mut_slice()[0] = MaybeUninit::new(43);
//!     reservation.commit();
//! }
//!
//! // Batch consume
//! let consumed = channel.consume_all(|item: &u64| {
//!     println!("Received: {}", item);
//! });
//! ```

mod backoff;
mod channel;
mod config;
mod metrics;
mod reservation;
mod ring;

pub use backoff::Backoff;
pub use channel::{Channel, ChannelError, Producer};
pub use config::{Config, HIGH_THROUGHPUT_CONFIG, LOW_LATENCY_CONFIG};
pub use metrics::Metrics;
pub use reservation::Reservation;
pub use ring::Ring;
