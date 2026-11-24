//! Async sender implementing `futures::Sink`.

use crate::error::StreamError;
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

/// A `Sink` sender's wait for ring space, kept across polls (#47).
///
/// A `Notified` that is dropped while pending leaves the `Notify` waiter list,
/// so a wait made of a local `Notified` registers nothing once `poll_ready` or
/// `poll_flush` returns `Pending`. This future owns its `Arc<Notify>`, so the
/// sender can store it and the receiver's `notify_waiters()` reaches it.
type SpaceWait = Pin<Box<dyn Future<Output = ()> + Send + Sync>>;

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
        producer: ExclusiveProducer<T>,
        data_notify: Arc<Notify>,
        backpressure_notify: Arc<Notify>,
        shutdown_state: Arc<ShutdownState>,
        pending_item: Option<T>,
        // The `Sink` path's wait for space, if it is waiting.
        space_wait: Option<SpaceWait>,
    }
}

/// Holds the `Producer` in its own module, so the tuple field is private to
/// it: nothing else in this file can reach `Producer::push` / `send` through
/// `&self`, and the `unsafe impl Sync` below stays true by construction.
mod exclusive {
    use ringmpsc_rs::{Producer, Reservation};

    /// A `Producer` reached only through `&mut self` for writes.
    ///
    /// `Producer` is `!Sync` because its `push` and `send` take `&self`. This
    /// wrapper doesn't expose them: it forwards `reserve` through `&mut self`, and
    /// through `&self` only `is_closed`, `close` and `id`, which touch nothing
    /// but atomics and a plain id. So it is safe to share, and `RingSender` gets its `Sync` from the
    /// compiler, which still checks every other field.
    pub(super) struct ExclusiveProducer<T>(Producer<T>);

    // Safety: a shared `&ExclusiveProducer` can only call `is_closed`, `close`
    // and `id`, which are atomic loads and stores and a read of a plain id. Every write to the ring goes through
    // `&mut self`, so it stays on one thread at a time and the ring keeps a single
    // producer (ringmpsc INV-SW-01). Keep it that way: never add a `&self` method
    // that reserves, pushes or sends, and never hand out `&Producer`. `T: Send`
    // matches `Producer`'s own `Send` bound.
    unsafe impl<T: Send> Sync for ExclusiveProducer<T> {}

    impl<T> ExclusiveProducer<T> {
        #[inline]
        pub(super) fn new(producer: Producer<T>) -> Self {
            Self(producer)
        }

        #[inline]
        pub(super) fn reserve(&mut self, n: usize) -> Option<Reservation<'_, T>> {
            self.0.reserve(n)
        }

        #[inline]
        pub(super) fn is_closed(&self) -> bool {
            self.0.is_closed()
        }

        /// The producer's id, which picks its send gate.
        #[inline]
        pub(super) fn id(&self) -> usize {
            self.0.id()
        }

        #[inline]
        pub(super) fn close(&self) {
            self.0.close();
        }
    }
}
use exclusive::ExclusiveProducer;

/// The outcome of one attempt to put an item in the ring.
enum Push<T> {
    /// Committed, and the receiver notified.
    Sent,
    /// The ring is full; the item is handed back.
    Full(T),
    /// The channel or this ring is closed; the item is handed back.
    Closed(T),
}

/// One non-blocking attempt to commit `item`. Every send path goes through
/// here, so every commit happens inside a send section of this producer's
/// gate: the receiver doesn't end the stream while one is open, so an item
/// this reports as `Sent` is always received (INV-SHUT-04). The gate is this
/// producer's own cache line, so senders don't contend with each other.
fn push_one<T>(
    producer: &mut ExclusiveProducer<T>,
    shutdown_state: &ShutdownState,
    data_notify: &Notify,
    item: T,
) -> Push<T> {
    // Fails once the channel is closed. While the section is open, the
    // receiver waits before its final drain.
    let Some(section) = shutdown_state.begin_send(producer.id()) else {
        #[cfg(debug_assertions)]
        debug_assert_item_preserved!(true, true);
        return Push::Closed(item);
    };
    if let Some(mut reservation) = producer.reserve(1) {
        if reservation.push(item).is_err() {
            unreachable!("a one-slot reservation has room for one item");
        }
        reservation.commit();
        // End the section after the commit: its Release publishes the item
        // to a receiver that sees the gates quiet. The notify below also
        // wakes a receiver waiting for that.
        let _ = section.end();
        data_notify.notify_one();
        // INV-SINK-03: Verify data notification sent
        #[cfg(debug_assertions)]
        debug_assert_data_notified!(true, true);
        return Push::Sent;
    }
    if section.end() {
        // The channel closed while this section was open; wake a receiver
        // waiting for the gates to go quiet.
        data_notify.notify_one();
    }
    // INV-SINK-01: the item was not consumed, since reserve() returned None.
    #[cfg(debug_assertions)]
    debug_assert_item_preserved!(true, true);
    // A closed ring refuses every reserve, so `None` may mean closed, not
    // full. Waiting for space would never end then.
    if producer.is_closed() {
        Push::Closed(item)
    } else {
        Push::Full(item)
    }
}

/// The `Sink` path: commit `pending_item`, or wait for space without losing
/// the wake-up (#47).
///
/// Every new wait is polled once (which registers this task) and then the
/// item is tried again, so a drain or close that lands between a failed try
/// and the registration is still seen. That retry happens for each new wait,
/// not just the first in a call.
fn poll_pending<T>(
    producer: &mut ExclusiveProducer<T>,
    shutdown_state: &ShutdownState,
    data_notify: &Notify,
    backpressure_notify: &Arc<Notify>,
    pending_item: &mut Option<T>,
    space_wait: &mut Option<SpaceWait>,
    cx: &mut Context<'_>,
) -> Poll<Result<(), StreamError>> {
    loop {
        let Some(item) = pending_item.take() else {
            *space_wait = None;
            return Poll::Ready(Ok(()));
        };
        match push_one(producer, shutdown_state, data_notify, item) {
            Push::Sent => {
                *space_wait = None;
                return Poll::Ready(Ok(()));
            }
            Push::Closed(item) => {
                // Keep the item: it was never sent.
                *pending_item = Some(item);
                *space_wait = None;
                return Poll::Ready(Err(StreamError::Closed));
            }
            Push::Full(item) => {
                *pending_item = Some(item);
                let created = space_wait.is_none();
                let wait = space_wait.get_or_insert_with(|| {
                    let notify = Arc::clone(backpressure_notify);
                    Box::pin(async move { notify.notified().await })
                });
                match wait.as_mut().poll(cx) {
                    // Woken by a drain or a close: try again.
                    Poll::Ready(()) => *space_wait = None,
                    // Just registered: try again, in case the wake-up came
                    // between the failed try and the registration.
                    Poll::Pending if created => {}
                    // Registered on an earlier pass, and the try after it
                    // failed too: wait.
                    Poll::Pending => return Poll::Pending,
                }
            }
        }
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
            producer: ExclusiveProducer::new(producer),
            data_notify,
            backpressure_notify,
            shutdown_state,
            pending_item: None,
            space_wait: None,
        }
    }

    /// Attempts to send an item without blocking.
    ///
    /// Returns `Ok(())` if the item was sent, or `Err(item)` if the
    /// ring is full or closed. The item is returned on failure.
    pub fn try_send(&mut self, item: T) -> Result<(), T> {
        match push_one(&mut self.producer, &self.shutdown_state, &self.data_notify, item) {
            Push::Sent => Ok(()),
            Push::Full(item) | Push::Closed(item) => Err(item),
        }
    }

    /// Sends an item, waiting for space if the ring is full.
    ///
    /// This is a convenience method for simple async sending.
    /// Uses reserve/commit internally to ensure no item loss.
    ///
    /// # Errors
    ///
    /// [`StreamError::Closed`] if the channel or this sender is closed,
    /// including while waiting for space. `Ok` means the receiver will yield
    /// the item.
    pub async fn send(&mut self, mut item: T) -> Result<(), StreamError> {
        loop {
            // Create the `Notified` before trying, so a wake-up between the
            // failed try and the await isn't lost: `notify_waiters()` (from
            // the receiver after a drain, or from a close) completes every
            // `Notified` created before it.
            //
            // BACKPRESSURE FLOW: the receiver calls
            // `backpressure_notify.notify_waiters()` after it drains items, and
            // `SenderFactory::close()` / shutdown call it after closing. Either
            // wakes this await, and the loop tries again.
            let notified = std::pin::pin!(self.backpressure_notify.notified());
            match push_one(&mut self.producer, &self.shutdown_state, &self.data_notify, item) {
                Push::Sent => return Ok(()),
                Push::Closed(_) => return Err(StreamError::Closed),
                Push::Full(back) => {
                    item = back;
                    notified.await;
                }
            }
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

    /// Takes back the item a `Sink` `start_send` / `feed` accepted but
    /// couldn't commit yet, if any.
    ///
    /// `Ok` from `start_send` / `feed` means the item was *accepted*: it may
    /// still be waiting here for ring space. It is delivered only once
    /// `poll_flush` / `flush` returns `Ok`. If the channel closes first,
    /// `flush` returns `Closed` and the item stays here, unsent; this
    /// returns it, so it isn't lost silently (INV-SINK-01).
    pub fn take_pending(&mut self) -> Option<T> {
        self.space_wait = None;
        self.pending_item.take()
    }
}

impl<T: Send + 'static> Sink<T> for RingSender<T> {
    type Error = StreamError;

    /// Checks if the sink is ready to accept an item.
    ///
    /// # When is this called?
    ///
    /// This is called by combinators like `SinkExt::send()`, `SinkExt::send_all()`,
    /// and `SinkExt::feed()` BEFORE calling `start_send()`. The typical pattern is:
    ///
    /// ```ignore
    /// // SinkExt::send() internally does:
    /// futures::future::poll_fn(|cx| sink.poll_ready(cx)).await?;
    /// sink.start_send(item)?;
    /// futures::future::poll_fn(|cx| sink.poll_flush(cx)).await?;
    /// ```
    ///
    /// # Behavior
    ///
    /// - Returns `Poll::Ready(Ok(()))` when the sink can accept a new item
    /// - Returns `Poll::Pending` if there's a pending item waiting for ring space
    /// - Returns `Poll::Ready(Err(...))` if the channel is closed
    ///
    /// If a previous `start_send()` stored a pending item (ring was full), this
    /// method attempts to flush it before declaring readiness. While it waits,
    /// it stays registered for the receiver's wake-up (#47).
    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.project();
        if this.shutdown_state.is_closed() || this.producer.is_closed() {
            return Poll::Ready(Err(StreamError::Closed));
        }
        poll_pending(
            this.producer,
            this.shutdown_state,
            this.data_notify,
            this.backpressure_notify,
            this.pending_item,
            this.space_wait,
            cx,
        )
    }

    /// Begins the process of sending an item to the sink.
    ///
    /// # When is this called?
    ///
    /// Called by combinators AFTER `poll_ready()` returns `Poll::Ready(Ok(()))`.
    ///
    /// ```ignore
    /// sink.poll_ready(cx).await?;
    /// sink.start_send(item)?;       // <-- THIS METHOD
    /// ```
    ///
    /// # Behavior
    ///
    /// - If ring has space: reserves, commits item, notifies receiver via `data_notify`
    /// - If ring is full: stores item in `pending_item` for later flush
    /// - Never blocks - always returns immediately
    ///
    /// # Note
    ///
    /// The item is NOT guaranteed to be in the ring after this call returns.
    /// It may be buffered in `pending_item`. Call `poll_flush()` to ensure delivery:
    /// only its `Ok` means the item will be received (INV-SHUT-04). If the
    /// channel closes first, `poll_flush()` returns `Closed` and the item can
    /// be recovered with [`RingSender::take_pending`].
    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        let this = self.project();
        match push_one(this.producer, this.shutdown_state, this.data_notify, item) {
            Push::Sent => Ok(()),
            Push::Full(item) => {
                // Ring is full - store as pending
                *this.pending_item = Some(item);
                Ok(())
            }
            Push::Closed(item) => {
                // Keep the unsent item, so `take_pending()` can hand it back
                // (INV-SINK-01): a close can land between `poll_ready` and
                // this call.
                *this.pending_item = Some(item);
                Err(StreamError::Closed)
            }
        }
    }

    /// Flushes any pending item to the ring buffer.
    ///
    /// # When is this called?
    ///
    /// Called by `SinkExt::send()` and `SinkExt::flush()` after `start_send()`
    /// to ensure the item is actually committed to the ring.
    ///
    /// ```ignore
    /// // SinkExt::send() does:
    /// sink.poll_ready(cx).await?;
    /// sink.start_send(item)?;
    /// sink.poll_flush(cx).await?;  // <-- THIS METHOD
    ///
    /// // SinkExt::feed() skips flush (batching optimization)
    /// // SinkExt::flush() explicitly calls this
    /// ```
    ///
    /// # Behavior
    ///
    /// - If no pending item: returns `Poll::Ready(Ok(()))` immediately
    /// - If pending item exists and ring has space: commits it, returns Ready
    /// - If pending item exists and ring is full: waits on `backpressure_notify`
    ///
    /// # Backpressure
    ///
    /// After a close, `poll_flush` returns `Ok` if nothing is pending (every
    /// accepted item was committed and will be received), and `Closed` if an
    /// item is still pending (recover it with [`RingSender::take_pending`]).
    ///
    /// When the ring is full, this method stores a wait for the receiver's
    /// `notify_waiters()` (sent after it drains items, or on close) and returns
    /// `Poll::Pending`. The stored wait keeps the task registered across polls
    /// (#47).
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.project();
        // Nothing pending: everything `start_send` accepted is committed and
        // will be received (INV-SHUT-04), so the flush succeeds even if the
        // channel has closed since. Reporting `Closed` here would make the
        // caller treat delivered items as lost.
        if this.pending_item.is_none() {
            *this.space_wait = None;
            return Poll::Ready(Ok(()));
        }
        // An item is still pending and the channel is closed: it can't be
        // sent; it stays in `pending_item` for `take_pending()`.
        if this.shutdown_state.is_closed() || this.producer.is_closed() {
            return Poll::Ready(Err(StreamError::Closed));
        }
        poll_pending(
            this.producer,
            this.shutdown_state,
            this.data_notify,
            this.backpressure_notify,
            this.pending_item,
            this.space_wait,
            cx,
        )
    }

    /// Closes the sink, flushing any pending item first.
    ///
    /// # When is this called?
    ///
    /// Called by `SinkExt::close()` to gracefully shut down the sink.
    ///
    /// ```ignore
    /// // SinkExt::close() does:
    /// sink.poll_close(cx).await?;  // <-- THIS METHOD
    ///
    /// // Also called when a Sink is dropped in some combinator contexts
    /// ```
    ///
    /// # Behavior
    ///
    /// 1. First calls `poll_flush()` to ensure any pending item is committed
    /// 2. Then closes the underlying producer ring
    /// 3. After close, further sends will fail with `StreamError::Closed`
    ///
    /// # Note
    ///
    /// This only closes THIS sender's ring. Other senders (from `factory.register()`)
    /// and the receiver continue operating. The channel itself remains open until
    /// `factory.close()` is called or all senders are dropped.
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
    /// Kept for compatibility: it behaves exactly like [`RingSender::send`],
    /// which no longer needs a clone per attempt.
    ///
    /// # Errors
    ///
    /// As for [`RingSender::send`].
    pub async fn send_cloned(&mut self, item: T) -> Result<(), StreamError> {
        self.send(item).await
    }
}

#[cfg(test)]
mod sync_tests {
    use super::RingSender;
    use futures::FutureExt;
    use ringmpsc_rs::Config;
    use tokio_stream::StreamExt;

    #[test]
    fn ring_sender_is_send_and_sync() {
        fn assert_send_sync<S: Send + Sync>() {}
        assert_send_sync::<RingSender<u64>>();
    }

    /// INV-SHUT-04, deterministically: a send that reserved before the close
    /// but commits after it is still received. The receiver must not end the
    /// stream while that send's section is open.
    #[tokio::test]
    async fn receiver_waits_for_an_open_send_section() {
        let (factory, mut rx) = crate::channel::<u64>(Config::new(3, 1, false));
        let mut tx = factory.register().expect("one sender");

        // The sender is mid-send: section open, slot reserved and written.
        let section = tx
            .shutdown_state
            .begin_send(tx.producer.id())
            .expect("channel is open");
        let mut reservation = tx.producer.reserve(1).expect("empty ring has room");
        assert!(reservation.push(7).is_ok());

        factory.close();
        assert!(
            rx.next().now_or_never().is_none(),
            "receiver ended the stream while a send was still committing"
        );

        reservation.commit();
        let _ = section.end();

        assert_eq!(rx.next().await, Some(7), "the in-flight send is received");
        assert_eq!(rx.next().await, None);
    }
}
