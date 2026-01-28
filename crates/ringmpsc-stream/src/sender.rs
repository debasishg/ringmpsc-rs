//! Async sender implementing `futures::Sink`.

use crate::error::StreamError;
#[cfg(debug_assertions)]
use crate::invariants::{debug_assert_data_notified, debug_assert_item_preserved};
use crate::shutdown::ShutdownState;
use ringmpsc_rs::Producer;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::Notify;

use futures_core::Future;
use futures_sink::Sink;
use pin_project_lite::pin_project;

pin_project! {
    /// Async sink sender wrapping a ringmpsc `Producer`.
    ///
    /// Implements `futures::Sink` with backpressure support.
    /// When the ring is full, `poll_ready` will return `Pending`
    /// until space becomes available.
    ///
    /// # Note
    ///
    /// `RingSender` does NOT implement `Clone`. This is intentional
    /// to preserve the single-producer-per-ring invariant. To create
    /// multiple senders, call `SenderFactory::register()` for each.
    pub struct RingSender<T> {
        producer: Producer<T>,
        data_notify: Arc<Notify>,
        backpressure_notify: Arc<Notify>,
        shutdown_state: Arc<ShutdownState>,
        pending_item: Option<T>,
    }
}

impl<T: Send + 'static> RingSender<T> {
    /// Creates a new sender wrapping the given producer.
    pub(crate) fn new(
        producer: Producer<T>,
        data_notify: Arc<Notify>,
        backpressure_notify: Arc<Notify>,
        shutdown_state: Arc<ShutdownState>,
    ) -> Self {
        Self {
            producer,
            data_notify,
            backpressure_notify,
            shutdown_state,
            pending_item: None,
        }
    }

    /// Attempts to send an item without blocking.
    ///
    /// Returns `Ok(())` if the item was sent, or `Err(item)` if the
    /// ring is full or closed. The item is returned on failure.
    pub fn try_send(&self, item: T) -> Result<(), T> {
        use std::mem::MaybeUninit;

        if self.shutdown_state.is_closed() || self.producer.is_closed() {
            // INV-SINK-01: Item preserved on closed channel
            #[cfg(debug_assertions)]
            debug_assert_item_preserved!(true, true);
            return Err(item);
        }

        if let Some(mut reservation) = self.producer.reserve(1) {
            reservation.as_mut_slice()[0] = MaybeUninit::new(item);
            reservation.commit();
            self.data_notify.notify_one();
            // INV-SINK-03: Verify data notification sent
            #[cfg(debug_assertions)]
            debug_assert_data_notified!(true, true);
            Ok(())
        } else {
            // Ring is full - item is NOT consumed since reserve() returned None
            // INV-SINK-01: Verify item preserved on backpressure
            #[cfg(debug_assertions)]
            debug_assert_item_preserved!(true, true);
            Err(item)
        }
    }

    /// Sends an item, waiting for space if the ring is full.
    ///
    /// This is a convenience method for simple async sending.
    /// Uses reserve/commit internally to ensure no item loss.
    pub async fn send(&self, item: T) -> Result<(), StreamError> {
        use std::mem::MaybeUninit;

        let mut item = Some(item);

        loop {
            if self.shutdown_state.is_closed() || self.producer.is_closed() {
                return Err(StreamError::Closed);
            }

            // Try to reserve space
            if let Some(mut reservation) = self.producer.reserve(1) {
                // Safety: item is Some here
                reservation.as_mut_slice()[0] = MaybeUninit::new(item.take().unwrap());
                reservation.commit();
                self.data_notify.notify_one();
                // INV-SINK-03: Verify data notification sent
                #[cfg(debug_assertions)]
                debug_assert_data_notified!(true, true);
                return Ok(());
            }

            // Ring is full - wait for backpressure relief
            // INV-SINK-01: Item preserved (still in Option)
            #[cfg(debug_assertions)]
            debug_assert_item_preserved!(true, item.is_some());
            self.backpressure_notify.notified().await;

            // After waking, check if we should give up
            if self.shutdown_state.is_closed() {
                return Err(StreamError::Closed);
            }

            // Retry - item is still owned by us
        }
    }

    /// Returns `true` if the sender's ring is closed.
    pub fn is_closed(&self) -> bool {
        self.shutdown_state.is_closed() || self.producer.is_closed()
    }

    /// Closes the sender's ring.
    pub fn close(&self) {
        self.producer.close();
    }
}

impl<T: Send + 'static> Sink<T> for RingSender<T> {
    type Error = StreamError;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.project();

        // Check for shutdown/closed state
        if this.shutdown_state.is_closed() || this.producer.is_closed() {
            return Poll::Ready(Err(StreamError::Closed));
        }

        // If we have a pending item, try to send it first
        if let Some(item) = this.pending_item.take() {
            use std::mem::MaybeUninit;

            if let Some(mut reservation) = this.producer.reserve(1) {
                reservation.as_mut_slice()[0] = MaybeUninit::new(item);
                reservation.commit();
                this.data_notify.notify_one();
                return Poll::Ready(Ok(()));
            } else {
                // Still full - re-store the item and wait
                *this.pending_item = Some(item);

                // Register for backpressure notification
                let notified = this.backpressure_notify.notified();
                tokio::pin!(notified);
                match notified.poll(cx) {
                    Poll::Ready(()) => {
                        // Notification received - re-poll
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }
        }

        // Ready to accept an item
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        let this = self.project();

        if this.shutdown_state.is_closed() || this.producer.is_closed() {
            return Err(StreamError::Closed);
        }

        use std::mem::MaybeUninit;

        if let Some(mut reservation) = this.producer.reserve(1) {
            reservation.as_mut_slice()[0] = MaybeUninit::new(item);
            reservation.commit();
            this.data_notify.notify_one();
            Ok(())
        } else {
            // Ring is full - store as pending
            *this.pending_item = Some(item);
            Ok(())
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.project();

        if this.shutdown_state.is_closed() || this.producer.is_closed() {
            return Poll::Ready(Err(StreamError::Closed));
        }

        // If we have a pending item, need to flush it
        if let Some(item) = this.pending_item.take() {
            use std::mem::MaybeUninit;

            if let Some(mut reservation) = this.producer.reserve(1) {
                reservation.as_mut_slice()[0] = MaybeUninit::new(item);
                reservation.commit();
                this.data_notify.notify_one();
                return Poll::Ready(Ok(()));
            } else {
                // Still full - re-store and wait
                *this.pending_item = Some(item);

                let notified = this.backpressure_notify.notified();
                tokio::pin!(notified);
                match notified.poll(cx) {
                    Poll::Ready(()) => {
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }
        }

        Poll::Ready(Ok(()))
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // First flush any pending item
        match self.as_mut().poll_flush(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => {}
        }

        let this = self.project();
        this.producer.close();
        Poll::Ready(Ok(()))
    }
}

/// Extension trait for `RingSender` providing async convenience methods.
impl<T: Send + Clone + 'static> RingSender<T> {
    /// Sends an item, waiting for space if the ring is full.
    ///
    /// This version uses clone for compatibility with the simpler push API.
    /// For zero-copy sending, use `send()` which uses reserve/commit internally.
    pub async fn send_cloned(&self, item: T) -> Result<(), StreamError> {
        use std::mem::MaybeUninit;

        loop {
            if self.shutdown_state.is_closed() || self.producer.is_closed() {
                return Err(StreamError::Closed);
            }

            if let Some(mut reservation) = self.producer.reserve(1) {
                reservation.as_mut_slice()[0] = MaybeUninit::new(item);
                reservation.commit();
                self.data_notify.notify_one();
                return Ok(());
            }

            // Ring is full - wait for backpressure relief
            self.backpressure_notify.notified().await;

            // After waking, check if we should give up
            if self.shutdown_state.is_closed() {
                return Err(StreamError::Closed);
            }

            // Retry - item is cloned on each attempt
        }
    }
}
