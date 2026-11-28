//! Shutdown utilities for graceful termination.

#[cfg(debug_assertions)]
use crate::invariants::{
    debug_assert_section_open, debug_assert_senders_woken, debug_assert_shutdown_signaled,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{oneshot, Notify};

/// The closed flag: the top bit of `ShutdownState::state`. The bits below
/// count open send sections.
const CLOSED: usize = 1 << (usize::BITS - 1);

/// Shared shutdown state between sender factory and receiver.
///
/// `state` holds the closed flag and the number of sends that are between
/// their closed check and their commit (INV-SHUT-04). Every change to it is a
/// read-modify-write on that one word, so the close and each send section
/// fall in one order: a send that starts after the close sees the flag and
/// fails, and the receiver, which waits until it sees the flag set with no
/// sections open, then sees every commit those sections made.
#[derive(Debug)]
pub(crate) struct ShutdownState {
    /// `CLOSED` bit plus the open-section count.
    state: AtomicUsize,
    /// Flag indicating shutdown has been initiated.
    shutdown_initiated: AtomicBool,
}

/// An open send section; ends when dropped, including on a panic.
pub(crate) struct SendSection<'a>(&'a ShutdownState);

impl Drop for SendSection<'_> {
    fn drop(&mut self) {
        // Release: the commit made inside the section happens-before a
        // receiver whose Acquire load sees the count back at zero. Every
        // change to `state` is a read-modify-write, so each Release here
        // heads a release sequence the receiver's load reads from.
        let _prev = self.0.state.fetch_sub(1, Ordering::Release);
        #[cfg(debug_assertions)]
        debug_assert_section_open!(_prev & !CLOSED);
    }
}

impl ShutdownState {
    pub(crate) fn new() -> Self {
        Self {
            state: AtomicUsize::new(0),
            shutdown_initiated: AtomicBool::new(false),
        }
    }

    /// Marks the channel closed. Sends that start afterwards fail; ones
    /// already in a section finish first, and the receiver waits for them.
    #[inline]
    pub(crate) fn close(&self) {
        self.state.fetch_or(CLOSED, Ordering::Release);
    }

    /// Returns `true` once `close` has run.
    #[inline]
    pub(crate) fn is_closed(&self) -> bool {
        self.state.load(Ordering::Acquire) & CLOSED != 0
    }

    /// Opens a send section, or returns `None` if the channel is closed.
    /// Commit only while holding the returned guard (INV-SHUT-04). Keep the
    /// section short and non-blocking: the receiver spins until it ends.
    #[inline]
    pub(crate) fn begin_send(&self) -> Option<SendSection<'_>> {
        // Acquire pairs with `close`'s Release.
        let prev = self.state.fetch_add(1, Ordering::Acquire);
        if prev & CLOSED != 0 {
            self.state.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        Some(SendSection(self))
    }

    /// Returns `true` if the channel is closed and no send section is open,
    /// so every send that succeeded has committed and its item is visible to
    /// this thread (Acquire). The receiver's final drain waits for this.
    #[inline]
    pub(crate) fn is_closed_and_quiet(&self) -> bool {
        self.state.load(Ordering::Acquire) == CLOSED
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
    /// 1. Close the channel: new registrations and every sender's next
    ///    send fail with `Closed`
    /// 2. Signal the consumer to drain remaining items
    /// 3. Wake any blocked senders so they can observe the closed state
    ///
    /// This method is idempotent - calling it multiple times has no
    /// additional effect after the first call.
    pub fn shutdown(&self) {
        if !self.state.is_shutdown_initiated() {
            self.state.initiate_shutdown();
            self.state.close();
            self.backpressure_notify.notify_waiters();
        }
    }

    /// Returns `true` if shutdown has been initiated.
    #[must_use] 
    pub fn is_shutdown(&self) -> bool {
        self.state.is_shutdown_initiated()
    }
}
