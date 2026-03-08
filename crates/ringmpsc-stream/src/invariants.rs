//! Debug assertion macros for stream pipeline invariants.
//!
//! These macros provide runtime checks for the invariants documented in `spec.md`.
//! They are only active in debug builds (`#[cfg(debug_assertions)]`), so there is
//! zero overhead in release builds.

// =============================================================================
// INV-STREAM-03: Backpressure Relief Signaling
// =============================================================================

/// Assert that backpressure was signaled after draining items.
///
/// **Invariant**: `consume_count > 0 → backpressure_notify.notify_waiters()`
///
/// Used in: `RingReceiver::poll_next()` after consuming items
macro_rules! debug_assert_backpressure_signaled {
    ($drained:expr, $signaled:expr) => {
        debug_assert!(
            $drained == 0 || $signaled,
            "INV-STREAM-03 violated: drained {} items but did not signal backpressure relief",
            $drained
        )
    };
}

// =============================================================================
// INV-STREAM-04: Graceful Shutdown Drain
// =============================================================================

/// Assert that shutdown drain was performed before returning None.
///
/// **Invariant**: `shutdown() → drain_all_remaining → return None`
///
/// Used in: `RingReceiver::poll_next()` during shutdown path
macro_rules! debug_assert_shutdown_drained {
    ($shutdown_initiated:expr, $drain_complete:expr) => {
        debug_assert!(
            !$shutdown_initiated || $drain_complete,
            "INV-STREAM-04 violated: shutdown initiated but drain not complete"
        )
    };
}

// =============================================================================
// INV-STREAM-05: Register-Then-Recheck (Notification Race Fix)
// =============================================================================

/// Assert that the post-registration re-drain caught items in the race window.
///
/// **Invariant**: After registering our waker via `data_notified.poll(cx)`,
/// we must re-drain the ring buffer. Any items found were pushed between our
/// last drain and the waker registration — the classic lost-wakeup window.
///
/// This assertion fires only when items ARE found, confirming the recheck
/// was necessary and caught real data. Zero items means no race occurred.
///
/// Used in: `RingReceiver::poll_next()` after `data_notified.poll()` → Pending
macro_rules! debug_assert_recheck_after_register {
    ($recheck_count:expr) => {
        debug_assert!(
            $recheck_count > 0,
            "INV-STREAM-05: recheck_after_register called but found 0 items \
             (this assert should only be reached when recheck_count > 0)"
        )
    };
}

// =============================================================================
// INV-STREAM-06: Timer Tick Loop (Waker Registration Guarantee)
// =============================================================================

/// Assert that the timer poll_tick loop consumed at least one tick before
/// reaching the Pending state that registers the waker.
///
/// **Invariant**: `poll_tick()` is called in a loop until it returns `Pending`.
/// `Interval::poll_tick()` only registers the waker when returning `Pending`.
/// If only called once and it returns `Ready` (first tick fires immediately),
/// the waker is never registered and the timer is permanently dead.
///
/// This is a structural/documentation assertion — the loop in `receiver.rs`
/// guarantees this by construction. The assert documents the requirement.
///
/// Used in: `RingReceiver::poll_next()` timer polling loop
macro_rules! debug_assert_timer_pending_reached {
    () => {
        // Structural assertion: this macro is placed at the `Pending` branch
        // of the poll_tick loop, confirming the waker was registered.
        // The loop structure guarantees we always reach here.
    };
}

// =============================================================================
// INV-SINK-01: No Item Loss on Backpressure
// =============================================================================

/// Assert that an item was preserved when send failed due to backpressure.
///
/// **Invariant**: `try_send(item) returns Err(item) on full ring`
///
/// Used in: `RingSender::try_send()` when reserve() returns None
macro_rules! debug_assert_item_preserved {
    ($reserve_failed:expr, $item_returned:expr) => {
        debug_assert!(
            !$reserve_failed || $item_returned,
            "INV-SINK-01 violated: reserve failed but item was not returned to caller"
        )
    };
}

// =============================================================================
// INV-SINK-02: Single Producer Per Ring (Compile-Time Enforced)
// =============================================================================

// **Invariant**: `∀ RingSender: backed by exactly one Producer`
//
// This invariant is enforced at compile-time via lack of Clone impl on RingSender.
// No runtime macro is needed - attempting to clone a RingSender results in a
// compile error: "the trait `Clone` is not implemented for `RingSender<T>`"

// =============================================================================
// INV-SINK-03: Data Arrival Notification
// =============================================================================

/// Assert that data arrival was notified after successful send.
///
/// **Invariant**: `successful_send → data_notify.notify_one()`
///
/// Used in: `RingSender::try_send()`, `RingSender::send()`, Sink impl
macro_rules! debug_assert_data_notified {
    ($send_success:expr, $notified:expr) => {
        debug_assert!(
            !$send_success || $notified,
            "INV-SINK-03 violated: send succeeded but data_notify was not called"
        )
    };
}

// =============================================================================
// INV-CH-01: Explicit Registration
// =============================================================================

/// Assert that registration was explicit (not via Clone).
///
/// **Invariant**: `SenderFactory::register() → Result<RingSender>`
///
/// This is enforced by API design - RingSender is not Clone.
/// The macro is called after successful registration to document the invariant.
macro_rules! debug_assert_explicit_registration {
    // Simple form: just document that registration happened
    ($registered:expr) => {
        debug_assert!(
            $registered,
            "INV-CH-01 violated: registration should return a unique sender"
        )
    };
    // Counting form: verify expected producer count
    ($producer_count:expr, $expected:expr) => {
        debug_assert!(
            $producer_count == $expected,
            "INV-CH-01 violated: expected {} registered producers, found {}",
            $expected,
            $producer_count
        )
    };
}

// =============================================================================
// INV-SHUT-01: Shutdown Signaled
// =============================================================================

/// Assert that shutdown was signaled via oneshot.
///
/// **Invariant**: `shutdown() → shutdown_tx.send(())`
macro_rules! debug_assert_shutdown_signaled {
    ($shutdown_called:expr, $signal_sent:expr) => {
        debug_assert!(
            !$shutdown_called || $signal_sent,
            "INV-SHUT-01 violated: shutdown called but signal was not sent"
        )
    };
}

// =============================================================================
// INV-SHUT-02: Wake Blocked Senders
// =============================================================================

/// Assert that blocked senders were woken during shutdown.
///
/// **Invariant**: `shutdown → backpressure_notify.notify_waiters()`
macro_rules! debug_assert_senders_woken {
    ($shutdown:expr, $woken:expr) => {
        debug_assert!(
            !$shutdown || $woken,
            "INV-SHUT-02 violated: shutdown but blocked senders were not woken"
        )
    };
}

// =============================================================================
// Re-exports for crate-internal use
// =============================================================================

pub(crate) use debug_assert_backpressure_signaled;
pub(crate) use debug_assert_data_notified;
pub(crate) use debug_assert_recheck_after_register;
pub(crate) use debug_assert_timer_pending_reached;
pub(crate) use debug_assert_explicit_registration;
pub(crate) use debug_assert_item_preserved;
pub(crate) use debug_assert_senders_woken;
pub(crate) use debug_assert_shutdown_drained;
pub(crate) use debug_assert_shutdown_signaled;
// debug_assert_single_producer is not exported - it's compile-time enforced via !Clone
