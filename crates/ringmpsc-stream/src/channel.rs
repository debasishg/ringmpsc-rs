//! Channel construction and sender factory.

use crate::config::StreamConfig;
use crate::error::StreamError;
use crate::receiver::RingReceiver;
use crate::sender::RingSender;
use crate::shutdown::ShutdownState;
use ringmpsc_rs::{Channel, Config};
use std::sync::Arc;
use tokio::sync::Notify;

#[cfg(debug_assertions)]
use crate::invariants::debug_assert_explicit_registration;

/// Creates a new async stream channel.
///
/// Returns a `SenderFactory` for creating senders and a `RingReceiver` for consuming items.
///
/// # Arguments
///
/// * `config` - The ringmpsc configuration (ring size, max producers, etc.)
///
/// # Example
///
/// ```ignore
/// use ringmpsc_stream::channel;
/// use ringmpsc_rs::Config;
///
/// let (factory, rx) = channel::<u64>(Config::default());
///
/// // Register senders explicitly
/// let tx1 = factory.register().unwrap();
/// let tx2 = factory.register().unwrap();
/// ```
pub fn channel<T: Send + 'static>(config: Config) -> (SenderFactory<T>, RingReceiver<T>) {
    channel_with_stream_config(config, StreamConfig::default())
}

/// Creates a new async stream channel with custom stream configuration.
///
/// # Arguments
///
/// * `config` - The ringmpsc configuration (ring size, max producers, etc.)
/// * `stream_config` - The stream-specific configuration (poll interval, batch hint)
pub fn channel_with_stream_config<T: Send + 'static>(
    config: Config,
    stream_config: StreamConfig,
) -> (SenderFactory<T>, RingReceiver<T>) {
    let channel = Arc::new(Channel::new(config));
    let data_notify = Arc::new(Notify::new());
    let backpressure_notify = Arc::new(Notify::new());
    let shutdown_state = Arc::new(ShutdownState::new());

    let receiver = RingReceiver::new(
        Arc::clone(&channel),
        Arc::clone(&data_notify),
        Arc::clone(&backpressure_notify),
        Arc::clone(&shutdown_state),
        stream_config,
    );

    let factory = SenderFactory {
        channel,
        data_notify,
        backpressure_notify,
        shutdown_state,
    };

    (factory, receiver)
}

/// Factory for creating `RingSender` instances.
///
/// Each call to `register()` creates a new sender backed by its own
/// ring buffer within the channel. This preserves the single-producer-per-ring
/// invariant that enables lock-free operation.
///
/// # Note
///
/// `SenderFactory` is `Clone`, allowing it to be shared across threads.
/// Each cloned factory can register its own senders.
#[derive(Clone)]
pub struct SenderFactory<T> {
    channel: Arc<Channel<T>>,
    data_notify: Arc<Notify>,
    backpressure_notify: Arc<Notify>,
    shutdown_state: Arc<ShutdownState>,
}

impl<T: Send + 'static> SenderFactory<T> {
    /// Registers a new sender.
    ///
    /// Each sender gets its own dedicated ring buffer within the channel.
    /// Returns an error if the maximum number of producers has been reached
    /// or if the channel is closed.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let (factory, rx) = channel::<u64>(Config::default());
    ///
    /// // Each producer thread registers its own sender
    /// let tx = factory.register().expect("registration failed");
    /// ```
    pub fn register(&self) -> Result<RingSender<T>, StreamError> {
        if self.shutdown_state.is_closed() {
            return Err(StreamError::Closed);
        }

        let producer = self.channel.register()?;

        // INV-CH-01: Explicit registration creates unique sender per ring
        #[cfg(debug_assertions)]
        debug_assert_explicit_registration!(true);

        Ok(RingSender::new(
            producer,
            Arc::clone(&self.data_notify),
            Arc::clone(&self.backpressure_notify),
            Arc::clone(&self.shutdown_state),
        ))
    }

    /// Closes the channel for new registrations.
    ///
    /// Existing senders can continue to send until their rings are full
    /// or they are dropped. New calls to `register()` will return
    /// `StreamError::Closed`.
    pub fn close(&self) {
        self.shutdown_state.close();
        self.channel.close();
    }

    /// Returns `true` if the channel is closed for new registrations.
    pub fn is_closed(&self) -> bool {
        self.shutdown_state.is_closed()
    }

    /// Returns the number of registered producers.
    pub fn producer_count(&self) -> usize {
        self.channel.producer_count()
    }
}
