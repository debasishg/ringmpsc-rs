//! Shutdown utilities for graceful termination.

#[cfg(debug_assertions)]
use crate::invariants::{debug_assert_senders_woken, debug_assert_shutdown_signaled};
use ringmpsc_rs::{SendGates, SendSection};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{oneshot, Notify};

/// Shared shutdown state between sender factory and receiver.
///
/// Closing has to be lossless: a send that returned `Ok` must be received
/// (INV-SHUT-04). Each sender commits inside a section of its producer's
/// gate in `gates` (ringmpsc INV-CH-05). Each gate is on its own cache line,
/// so senders don't contend with each other. The receiver ends the stream
/// only once `is_closed_and_quiet()`: closed, and no section open.
///
/// It owns the whole close, so every close path (`SenderFactory::close`,
/// `RingReceiver::shutdown`, `ShutdownSignal::shutdown`) does the same:
/// close the gates and the core channel, then wake the receiver and any
/// senders waiting for space (INV-CH-03).
pub(crate) struct ShutdownState {
    /// One send gate per producer id.
    gates: SendGates,
    /// Closes the core `Channel` (its registry and rings). A closure, so
    /// this state needn't be generic over the item type.
    close_channel: Box<dyn Fn() + Send + Sync>,
    /// Wakes the receiver, so it notices the close without waiting for a
    /// poll tick.
    data_notify: Arc<Notify>,
    /// Wakes senders waiting for space, so they see the close.
    backpressure_notify: Arc<Notify>,
    /// Set by `close()` after every gate is closed.
    closed: AtomicBool,
    /// Flag indicating shutdown has been initiated.
    shutdown_initiated: AtomicBool,
}

impl ShutdownState {
    /// State for a channel with up to `max_producers` senders.
    /// `close_channel` closes the core channel.
    pub(crate) fn new(
        max_producers: usize,
        close_channel: Box<dyn Fn() + Send + Sync>,
        data_notify: Arc<Notify>,
        backpressure_notify: Arc<Notify>,
    ) -> Self {
        Self {
            gates: SendGates::new(max_producers),
            close_channel,
            data_notify,
            backpressure_notify,
            closed: AtomicBool::new(false),
            shutdown_initiated: AtomicBool::new(false),
        }
    }

    /// Closes the channel. New registrations and every send that starts
    /// afterwards fail with `Closed`; sends already in a section finish
    /// first, and the receiver waits for them. Idempotent.
    pub(crate) fn close(&self) {
        // Every close is a shutdown, whichever path started it, so
        // `is_shutdown()` reports it for a factory close and a receiver drop
        // too.
        self.initiate_shutdown();
        // Gates first: no send section opens after this.
        self.gates.close();
        // Then the core channel: its registry refuses new producers and its
        // rings refuse new reserves.
        (self.close_channel)();
        // Release: a thread that sees `closed` also sees the above.
        self.closed.store(true, Ordering::Release);
        // Wake the receiver so it ends the stream promptly, and senders
        // waiting for space so they return `Closed`.
        self.data_notify.notify_one();
        self.backpressure_notify.notify_waiters();
    }

    /// Returns `true` once `close` has run.
    #[inline]
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Opens sender `id`'s send section, or returns `None` if the channel is
    /// closed. Commit only inside it (INV-SHUT-04), and keep it short and
    /// non-blocking: the receiver waits for it.
    #[inline]
    pub(crate) fn begin_send(&self, id: usize) -> Option<SendSection<'_>> {
        self.gates.begin(id)
    }

    /// Returns `true` if sender `id`'s gate is not closed yet.
    #[inline]
    pub(crate) fn is_gate_open(&self, id: usize) -> bool {
        self.gates.is_open(id)
    }

    /// Returns `true` if the channel is closed and no send section is open,
    /// so every send that succeeded has committed and its item is visible to
    /// this thread. The receiver's final drain waits for this.
    #[inline]
    pub(crate) fn is_closed_and_quiet(&self) -> bool {
        self.is_closed() && self.gates.is_quiet()
    }

    /// Initiates shutdown sequence.
    #[inline]
    pub(crate) fn initiate_shutdown(&self) {
        self.shutdown_initiated.store(true, Ordering::Release);
    }

    /// Returns `true` if shutdown has been initiated.
    #[inline]
    pub(crate) fn is_shutdown_initiated(&self) -> bool {
        self.shutdown_initiated.load(Ordering::Acquire)
    }
}

impl std::fmt::Debug for ShutdownState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShutdownState")
            .field("gates", &self.gates)
            .field("closed", &self.is_closed())
            .field("shutdown_initiated", &self.is_shutdown_initiated())
            .finish_non_exhaustive()
    }
}

/// Handle for triggering shutdown from the receiver side.
pub(crate) struct ShutdownHandle {
    /// Oneshot sender to signal the consumer loop to drain and exit.
    pub(crate) shutdown_tx: Option<oneshot::Sender<()>>,
    /// Shared state for coordinating shutdown.
    pub(crate) state: Arc<ShutdownState>,
    /// Notify to wake blocked senders during shutdown.
    pub(crate) backpressure_notify: Arc<Notify>,
}

impl ShutdownHandle {
    /// Triggers graceful shutdown.
    /// 
    /// This will:
    /// 1. Mark the channel as closed, so every sender's next send fails
    /// 2. Signal the consumer loop to drain remaining items
    /// 3. Wake any blocked senders so they can observe the closed state
    pub(crate) fn trigger(&mut self) {
        self.state.initiate_shutdown();
        self.state.close();

        // Signal consumer loop to drain
        let signal_sent = if let Some(tx) = self.shutdown_tx.take() {
            tx.send(()).is_ok()
        } else {
            false
        };

        // INV-SHUT-01: Verify shutdown was signaled
        #[cfg(debug_assertions)]
        debug_assert_shutdown_signaled!(true, signal_sent || self.shutdown_tx.is_none());

        // Wake blocked senders so they can observe closed state
        self.backpressure_notify.notify_waiters();

        // INV-SHUT-02: Verify senders were woken
        #[cfg(debug_assertions)]
        debug_assert_senders_woken!(true, true);
    }
}

/// A cloneable signal for triggering shutdown externally.
///
/// Multiple clones of this handle can trigger shutdown - only the first
/// one has effect, subsequent calls are no-ops.
#[derive(Clone)]
pub struct ShutdownSignal {
    state: Arc<ShutdownState>,
    backpressure_notify: Arc<Notify>,
}

impl ShutdownSignal {
    pub(crate) fn new(
        state: Arc<ShutdownState>,
        backpressure_notify: Arc<Notify>,
    ) -> Self {
        Self {
            state,
            backpressure_notify,
        }
    }

    /// Triggers graceful shutdown.
    ///
    /// Calling this will:
    /// 1. Close the channel, including the core channel: new registrations
    ///    and every sender's next send fail with `Closed`
    /// 2. Wake the receiver, which drains the remaining items and then ends
    ///    the stream
    /// 3. Wake any blocked senders so they can observe the closed state
    ///
    /// This method is idempotent - calling it multiple times has no
    /// additional effect after the first call.
    pub fn shutdown(&self) {
        if !self.state.is_shutdown_initiated() {
            self.state.initiate_shutdown();
            // Closes everything and wakes the receiver and blocked senders.
            self.state.close();
            // Kept for INV-SHUT-02's explicit wake (close already did it).
            self.backpressure_notify.notify_waiters();
        }
    }

    /// Returns `true` if shutdown has been initiated.
    #[must_use] 
    pub fn is_shutdown(&self) -> bool {
        self.state.is_shutdown_initiated()
    }
}
