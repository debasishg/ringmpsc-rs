//! Async receiver implementing `futures::Stream`.

use crate::config::StreamConfig;
#[cfg(debug_assertions)]
use crate::invariants::{
    debug_assert_backpressure_signaled, debug_assert_quiet_at_end,
    debug_assert_recheck_after_register, debug_assert_shutdown_drained,
};
use crate::shutdown::{ShutdownHandle, ShutdownSignal, ShutdownState};
use ringmpsc_rs::Consumer;
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
        // The channel's one consumer handle. Every drain goes through it.
        consumer: Consumer<T>,
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
        // The wait for new data (or a close), kept across polls.
        data_wait: Option<NotifyWait>,
        // After a close: the wait for the last open send sections to end.
        quiet_wait: Option<NotifyWait>,
    }
}

/// A wait on `data_notify`, kept across polls.
///
/// A `Notified` that is dropped while pending leaves the waiter list, so a
/// wait made of a local `Notified` registers nothing once `poll_next`
/// returns `Pending`: the receiver would only wake on its poll tick. This
/// future owns its `Arc<Notify>`, so the receiver stores it and a sender's
/// `notify_one()` (after a send, or after ending a section past a close) or
/// a close's notify wakes the task.
type NotifyWait = Pin<Box<dyn Future<Output = ()> + Send + Sync>>;

/// A new wait on `notify`, created unpolled.
fn notify_wait(notify: &Arc<Notify>) -> NotifyWait {
    let notify = Arc::clone(notify);
    Box::pin(async move { notify.notified().await })
}

impl<T: Send + 'static> RingReceiver<T> {
    /// Creates a new receiver draining through the channel's consumer handle.
    pub(crate) fn new(
        consumer: Consumer<T>,
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
            consumer,
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
            data_wait: None,
            quiet_wait: None,
        }
    }

    /// Initiates graceful shutdown.
    ///
    /// This will:
    /// 1. Close the channel: new registrations and every sender's next send
    ///    fail with `Closed`
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
    #[must_use] 
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
    #[must_use] 
    pub fn shutdown_signal(&self) -> ShutdownSignal {
        ShutdownSignal::new(
            Arc::clone(&self.shutdown_state),
            Arc::clone(&self.backpressure_notify),
        )
    }

    /// Returns the number of items currently buffered.
    #[must_use] 
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
                    return finish(
                        this.consumer,
                        this.buffer,
                        this.backpressure_notify,
                        this.data_notify,
                        this.shutdown_state,
                        this.drain_complete,
                        this.quiet_wait,
                        this.poll_timer.as_mut(),
                        cx,
                    );
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
                this.consumer.consume_all_up_to_owned(batch_limit, |item| {
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

        // Register for data notification, in a wait kept across polls so a
        // later `notify_one()` reaches this task (not just the poll tick).
        let wait = this
            .data_wait
            .get_or_insert_with(|| notify_wait(this.data_notify));

        // Check if data arrived while we were setting up
        match wait.as_mut().poll(cx) {
            Poll::Ready(()) => {
                *this.data_wait = None;
                *this.data_pending = true;
                // Re-poll to process the notification
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            Poll::Pending => {}
        }

        // INV-STREAM-05: Register-then-recheck — drain ring after waker
        // registration to close the race window where a producer pushes
        // an item + calls notify_one() between our last drain and the
        // notified().poll() call above. Without this re-drain, the
        // notification would wake our task, but on re-entry data_pending
        // is false so we'd skip the drain and wait for the timer.
        {
            let batch_limit = this.config.batch_hint.saturating_sub(this.buffer.len());
            if batch_limit > 0 {
                let mut recheck_count = 0;
                this.consumer.consume_all_up_to_owned(batch_limit, |item| {
                    this.buffer.push_back(item);
                    recheck_count += 1;
                });
                if recheck_count > 0 {
                    this.backpressure_notify.notify_waiters();
                    // INV-STREAM-03: Verify backpressure signaled
                    #[cfg(debug_assertions)]
                    debug_assert_backpressure_signaled!(recheck_count, true);
                    // INV-STREAM-05: Verify recheck caught items
                    #[cfg(debug_assertions)]
                    debug_assert_recheck_after_register!(recheck_count);
                    if let Some(item) = this.buffer.pop_front() {
                        return Poll::Ready(Some(item));
                    }
                }
            }
        }

        // Poll interval timer as safety net.
        // IMPORTANT: poll_tick only registers the waker when it returns Pending.
        // If it returns Ready (tick fired), we must loop to consume all elapsed
        // ticks and eventually get Pending, which registers for the next tick.
        // Without this loop, after the first Ready the timer is effectively dead
        // and the task is never re-woken by the timer.
        #[allow(clippy::while_let_loop)]
        loop {
            match this.poll_timer.as_mut().poll_tick(cx) {
                Poll::Ready(_) => {
                    // Timer fired - try to drain even without notification
                    let batch_limit =
                        this.config.batch_hint.saturating_sub(this.buffer.len());
                    if batch_limit > 0 {
                        let mut count = 0;
                        this.consumer.consume_all_up_to_owned(batch_limit, |item| {
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
                    // Continue loop to consume more elapsed ticks and register
                    // for the next one (poll_tick must return Pending to register).
                }
                Poll::Pending => break, // Waker registered for next tick (INV-STREAM-06)
            }
        }

        // Check if channel is closed
        if this.shutdown_state.is_closed() {
            return finish(
                this.consumer,
                this.buffer,
                this.backpressure_notify,
                this.data_notify,
                this.shutdown_state,
                this.drain_complete,
                this.quiet_wait,
                this.poll_timer.as_mut(),
                cx,
            );
        }

        Poll::Pending
    }
}

/// End of stream after a close: yield what is left, and return `None` only
/// once no send can still commit (INV-SHUT-04, INV-STREAM-04).
///
/// A sender that opened its send section before the close may not have
/// committed yet. `is_closed_and_quiet()` is true only once every section
/// has ended, and its Acquire makes their commits visible, so the drain
/// after it sees every item a send reported as `Ok`. Until then this waits
/// on `data_notify`, which a sender ending a section after the close
/// signals, in a wait stored across polls; each new wait is followed by one
/// more check, so a section that ends before the wait registers isn't missed.
/// The poll timer is a fallback, for a section ended by a panicking sender.
#[allow(clippy::too_many_arguments)]
fn finish<T>(
    consumer: &mut Consumer<T>,
    buffer: &mut VecDeque<T>,
    backpressure_notify: &Notify,
    data_notify: &Arc<Notify>,
    shutdown_state: &ShutdownState,
    drain_complete: &mut bool,
    quiet_wait: &mut Option<NotifyWait>,
    mut poll_timer: Pin<&mut Interval>,
    cx: &mut Context<'_>,
) -> Poll<Option<T>> {
    loop {
        let quiet = shutdown_state.is_closed_and_quiet();
        let mut drained = 0usize;
        consumer.consume_all_owned(|item| {
            buffer.push_back(item);
            drained += 1;
        });
        if drained > 0 {
            backpressure_notify.notify_waiters();
        }
        // INV-STREAM-03: Verify backpressure was signaled
        #[cfg(debug_assertions)]
        debug_assert_backpressure_signaled!(drained, true);

        if quiet {
            // Nothing can be committed any more, and this drain saw everything.
            *quiet_wait = None;
            *drain_complete = true;
            // INV-STREAM-04: Verify drain complete before returning None
            #[cfg(debug_assertions)]
            debug_assert_shutdown_drained!(true, *drain_complete);
            // INV-SHUT-04: still closed and quiet now that the stream ends.
            #[cfg(debug_assertions)]
            debug_assert_quiet_at_end!(shutdown_state.is_closed_and_quiet());
            return Poll::Ready(buffer.pop_front());
        }
        if let Some(item) = buffer.pop_front() {
            return Poll::Ready(Some(item));
        }

        // A send section is still open: wait for its end to notify us.
        let created = quiet_wait.is_none();
        let wait = quiet_wait.get_or_insert_with(|| notify_wait(data_notify));
        match wait.as_mut().poll(cx) {
            // Woken: check again.
            Poll::Ready(()) => *quiet_wait = None,
            // Just registered: check again, in case the section ended first.
            Poll::Pending if created => {}
            Poll::Pending => {
                // Fallback wake-up: a section ended by a panicking sender
                // (its guard's Drop) can't notify, so also wait for the next
                // poll tick (INV-STREAM-06: loop until Pending registers it).
                while poll_timer.as_mut().poll_tick(cx).is_ready() {}
                return Poll::Pending;
            }
        }
    }
}

