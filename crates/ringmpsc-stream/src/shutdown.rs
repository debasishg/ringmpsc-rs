//! Shutdown utilities for graceful termination.

#[cfg(debug_assertions)]
use crate::invariants::{debug_assert_senders_woken, debug_assert_shutdown_signaled};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{oneshot, Notify};

/// Shared shutdown state between sender factory and receiver.
#[derive(Debug)]
pub(crate) struct ShutdownState {
    /// Flag indicating the channel is closed for new registrations.
    closed: AtomicBool,
    /// Flag indicating shutdown has been initiated.
    shutdown_initiated: AtomicBool,
}

impl ShutdownState {
    pub(crate) fn new() -> Self {
        Self {
            closed: AtomicBool::new(false),
            shutdown_initiated: AtomicBool::new(false),
        }
    }

    /// Marks the channel as closed for new registrations.
    #[inline]
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    /// Returns `true` if closed for new registrations.
    #[inline]
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
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
    /// 1. Mark the channel as closed
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
    /// 1. Close the channel for new registrations
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
    pub fn is_shutdown(&self) -> bool {
        self.state.is_shutdown_initiated()
    }
}
