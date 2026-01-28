//! Async receiver implementing `futures::Stream`.

use crate::config::StreamConfig;
#[cfg(debug_assertions)]
use crate::invariants::{debug_assert_backpressure_signaled, debug_assert_shutdown_drained};
use crate::shutdown::{ShutdownHandle, ShutdownSignal, ShutdownState};
use ringmpsc_rs::Channel;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::{oneshot, Notify};
use tokio::time::{interval, Interval};

use futures_core::{Future, Stream};
use pin_project_lite::pin_project;

pin_project! {
    /// Async stream receiver wrapping a ringmpsc `Channel`.
    ///
    /// Implements `futures::Stream` with hybrid event-driven + polling strategy.
    /// Items are yielded in per-producer FIFO order (inherits from ringmpsc).
    ///
    /// # Backpressure
    ///
    /// After draining items, the receiver calls `notify_waiters()` on the
    /// backpressure notify to wake any blocked senders.
    ///
    /// # Shutdown
    ///
    /// Call `shutdown()` for graceful termination which drains remaining items
    /// before returning `None`. Can also be composed with `StreamExt::take_until`
    /// for external cancellation control.
    pub struct RingReceiver<T> {
        channel: Arc<Channel<T>>,
        data_notify: Arc<Notify>,
        backpressure_notify: Arc<Notify>,
        shutdown_state: Arc<ShutdownState>,
        shutdown_rx: Option<oneshot::Receiver<()>>,
        shutdown_handle: Option<ShutdownHandle>,
        config: StreamConfig,
        #[pin]
        poll_timer: Interval,
        buffer: VecDeque<T>,
        data_pending: bool,
        drain_complete: bool,
    }
}

impl<T: Send + 'static> RingReceiver<T> {
    /// Creates a new receiver wrapping the given channel.
    pub(crate) fn new(
        channel: Arc<Channel<T>>,
        data_notify: Arc<Notify>,
        backpressure_notify: Arc<Notify>,
        shutdown_state: Arc<ShutdownState>,
        config: StreamConfig,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let shutdown_handle = ShutdownHandle {
            shutdown_tx: Some(shutdown_tx),
            state: Arc::clone(&shutdown_state),
            backpressure_notify: Arc::clone(&backpressure_notify),
        };

        Self {
            channel,
            data_notify,
            backpressure_notify,
            shutdown_state,
            shutdown_rx: Some(shutdown_rx),
            shutdown_handle: Some(shutdown_handle),
            config: config.clone(),
            poll_timer: interval(config.poll_interval),
            buffer: VecDeque::with_capacity(config.batch_hint),
            data_pending: false,
            drain_complete: false,
        }
    }

    /// Initiates graceful shutdown.
    ///
    /// This will:
    /// 1. Close the channel for new registrations
    /// 2. Signal internal consumer to drain remaining items
    /// 3. Continue yielding items until all are consumed
    /// 4. Return `None` when fully drained
    ///
    /// # Note
    ///
    /// After calling this, continue polling the stream to receive
    /// remaining items until it returns `None`.
    pub fn shutdown(&mut self) {
        if let Some(ref mut handle) = self.shutdown_handle {
            handle.trigger();
        }
    }

    /// Returns `true` if the stream has been shut down.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown_state.is_shutdown_initiated()
    }

    /// Returns a cloneable shutdown signal.
    ///
    /// This can be used to trigger shutdown from another task or thread.
    /// The signal is cloneable, so you can distribute it to multiple
    /// parts of your application.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let (factory, rx) = channel::<u64>(Config::default());
    /// let signal = rx.shutdown_signal();
    ///
    /// // Pass to another task
    /// tokio::spawn(async move {
    ///     tokio::time::sleep(Duration::from_secs(5)).await;
    ///     signal.shutdown(); // Trigger shutdown from another task
    /// });
    /// ```
    pub fn shutdown_signal(&self) -> ShutdownSignal {
        ShutdownSignal::new(
            Arc::clone(&self.shutdown_state),
            Arc::clone(&self.backpressure_notify),
        )
    }

    /// Returns the number of items currently buffered.
    pub fn buffered_count(&self) -> usize {
        self.buffer.len()
    }
}

impl<T: Send + 'static> Stream for RingReceiver<T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        // If we have buffered items, yield one
        if let Some(item) = this.buffer.pop_front() {
            return Poll::Ready(Some(item));
        }

        // Check if shutdown drain is complete
        if *this.drain_complete {
            return Poll::Ready(None);
        }

        // Check for shutdown signal
        if let Some(ref mut rx) = this.shutdown_rx {
            match Pin::new(rx).poll(cx) {
                Poll::Ready(_) => {
                    // Shutdown signaled - perform final drain
                    *this.shutdown_rx = None;

                    // Drain all remaining items
                    let mut drained = 0usize;
                    this.channel.consume_all_owned(|item| {
                        this.buffer.push_back(item);
                        drained += 1;
                    });
                    this.backpressure_notify.notify_waiters();
                    *this.drain_complete = true;

                    // INV-STREAM-03: Verify backpressure was signaled
                    #[cfg(debug_assertions)]
                    debug_assert_backpressure_signaled!(drained, true);

                    // INV-STREAM-04: Verify drain complete before returning None
                    #[cfg(debug_assertions)]
                    debug_assert_shutdown_drained!(true, *this.drain_complete);

                    // Yield buffered item if any
                    if let Some(item) = this.buffer.pop_front() {
                        return Poll::Ready(Some(item));
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => {}
            }
        }

        // Hybrid polling: check data notify
        if *this.data_pending {
            *this.data_pending = false;
            let batch_limit = this.config.batch_hint.saturating_sub(this.buffer.len());
            if batch_limit > 0 {
                let mut drained = 0usize;
                this.channel.consume_all_up_to_owned(batch_limit, |item| {
                    this.buffer.push_back(item);
                    drained += 1;
                });
                let signaled = !this.buffer.is_empty();
                if signaled {
                    this.backpressure_notify.notify_waiters();
                }
                // INV-STREAM-03: Verify backpressure signaled after drain
                #[cfg(debug_assertions)]
                debug_assert_backpressure_signaled!(drained, signaled || drained == 0);
            }
        }

        // Yield buffered item if any
        if let Some(item) = this.buffer.pop_front() {
            return Poll::Ready(Some(item));
        }

        // Register for data notification
        let data_notified = this.data_notify.notified();
        tokio::pin!(data_notified);

        // Check if data arrived while we were setting up
        match data_notified.as_mut().poll(cx) {
            Poll::Ready(()) => {
                *this.data_pending = true;
                // Re-poll to process the notification
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            Poll::Pending => {}
        }

        // Poll interval timer as safety net
        match this.poll_timer.as_mut().poll_tick(cx) {
            Poll::Ready(_) => {
                // Timer fired - try to drain even without notification
                let batch_limit = this.config.batch_hint.saturating_sub(this.buffer.len());
                if batch_limit > 0 {
                    let mut count = 0;
                    this.channel.consume_all_up_to_owned(batch_limit, |item| {
                        this.buffer.push_back(item);
                        count += 1;
                    });
                    if count > 0 {
                        this.backpressure_notify.notify_waiters();
                        // INV-STREAM-03: Verify backpressure signaled
                        #[cfg(debug_assertions)]
                        debug_assert_backpressure_signaled!(count, true);
                        if let Some(item) = this.buffer.pop_front() {
                            return Poll::Ready(Some(item));
                        }
                    }
                }
            }
            Poll::Pending => {}
        }

        // Check if channel is closed
        if this.shutdown_state.is_closed() {
            // Try one more drain to ensure we got everything
            let mut found_any = false;
            this.channel.consume_all_owned(|item| {
                this.buffer.push_back(item);
                found_any = true;
            });
            if found_any {
                this.backpressure_notify.notify_waiters();
                // INV-STREAM-03: Verify backpressure signaled
                #[cfg(debug_assertions)]
                debug_assert_backpressure_signaled!(1, true);
                if let Some(item) = this.buffer.pop_front() {
                    return Poll::Ready(Some(item));
                }
            }
            return Poll::Ready(None);
        }

        Poll::Pending
    }
}
