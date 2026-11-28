//! Producer registration and channel close, in one atomic word (INV-CH-04).
//!
//! `Channel` and `StackChannel` used to keep the producer count and the
//! closed flag in two atomics. That allowed two races (#45):
//!
//! - `register()` bumped the count before checking it against the limit and
//!   rolled back afterwards, so a reader could briefly see `max + 1` and
//!   slice `rings[..max + 1]` out of range.
//! - `register()` checked `closed` and then claimed an id, while `close()`
//!   set `closed` and then read the count. A `register()` running between
//!   the two could claim an id that `close()` never saw, so that ring was
//!   never closed.
//!
//! Here the count and a `CLOSED` bit share one word. `claim` is a CAS loop
//! that never takes the count past the limit, and `close` sets the bit with a
//! single `fetch_or`. Every `claim` and `close` is one atomic read-modify-
//! write on the same word, so they happen in one total order: a claim either
//! comes before the `fetch_or`, whose result then includes its id, or after
//! it, and sees the bit and fails. No `SeqCst` is needed for that.

use crate::invariants::debug_assert_registered_bound;

/// The closed flag: the top bit of the word. The count uses the bits below.
const CLOSED: usize = 1 << (usize::BITS - 1);

/// Why [`Registry::claim`] failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimError {
    /// The channel was closed.
    Closed,
    /// `max` producers are already registered.
    Full,
}

/// An atomic `usize`. Implemented for the std type and, under the `loom`
/// feature, for loom's, so the loom tests run this exact code.
pub trait AtomicWord {
    fn load_acquire(&self) -> usize;
    fn load_relaxed(&self) -> usize;
    /// `compare_exchange_weak` with `AcqRel` on success and `Acquire` on failure.
    fn cas_weak_acq_rel(&self, current: usize, new: usize) -> Result<usize, usize>;
    /// `fetch_or` with Release.
    fn fetch_or_release(&self, v: usize) -> usize;
}

macro_rules! impl_atomic_word {
    ($t:ty) => {
        impl AtomicWord for $t {
            #[inline]
            fn load_acquire(&self) -> usize {
                self.load(std::sync::atomic::Ordering::Acquire)
            }
            #[inline]
            fn load_relaxed(&self) -> usize {
                self.load(std::sync::atomic::Ordering::Relaxed)
            }
            #[inline]
            fn cas_weak_acq_rel(&self, current: usize, new: usize) -> Result<usize, usize> {
                self.compare_exchange_weak(
                    current,
                    new,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
            }
            #[inline]
            fn fetch_or_release(&self, v: usize) -> usize {
                self.fetch_or(v, std::sync::atomic::Ordering::Release)
            }
        }
    };
}

impl_atomic_word!(std::sync::atomic::AtomicUsize);
#[cfg(feature = "loom")]
impl_atomic_word!(loom::sync::atomic::AtomicUsize);

/// Producer count plus closed flag, as one atomic word (INV-CH-04).
///
/// Memory orderings, the minimum each use needs:
/// - **`close`: Release.** Everything the closing thread did before
///   `close()` happens-before any thread whose `claim` then fails with
///   `Closed`, or that sees the bit through `is_closed`.
/// - **A successful `claim`: `AcqRel`.** Release, so everything a registering
///   thread did before `register()` happens-before a thread that sees the new
///   count through `count_acquire` (the public `producer_count()`). Acquire,
///   to pair with `close` and with earlier claims.
/// - **`is_closed`, `count_acquire`, a failed `claim`: Acquire,** to pair
///   with those Releases.
/// - **`count`: Relaxed.** For internal slicing only. It only bounds an index
///   into the rings, which exist before the channel is shared. The bound comes
///   from the value itself (never above `max`, INV-CH-04), not from any
///   ordering, and each ring's own atomics order access to it.
pub struct Registry<W: AtomicWord = std::sync::atomic::AtomicUsize> {
    state: W,
}

impl Registry {
    /// An open registry with no producers.
    #[must_use]
    pub const fn new() -> Self {
        Self::from_word(std::sync::atomic::AtomicUsize::new(0))
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl<W: AtomicWord> Registry<W> {
    /// A registry over `state`, which must be 0 (open, no producers). Lets
    /// the loom tests pass a loom atomic.
    #[must_use]
    pub const fn from_word(state: W) -> Self {
        Self { state }
    }

    /// Claims the next producer id, below `max`.
    ///
    /// # Errors
    ///
    /// [`ClaimError::Closed`] if the channel is closed, else
    /// [`ClaimError::Full`] if `max` ids are taken. The count never goes
    /// past `max`, not even for an instant (INV-CH-04).
    pub fn claim(&self, max: usize) -> Result<usize, ClaimError> {
        debug_assert!(max < CLOSED, "max_producers must leave the top bit free");
        let mut current = self.state.load_acquire();
        loop {
            if current & CLOSED != 0 {
                return Err(ClaimError::Closed);
            }
            // The bit is clear, so `current` is the count.
            if current >= max {
                return Err(ClaimError::Full);
            }
            match self.state.cas_weak_acq_rel(current, current + 1) {
                Ok(_) => {
                    let registered = current + 1;
                    debug_assert_registered_bound!(registered, max);
                    return Ok(current);
                }
                Err(actual) => current = actual,
            }
        }
    }

    /// Marks the channel closed and returns how many ids were claimed at
    /// that moment. Every `claim` that succeeded returned an id below this
    /// number, and every later `claim` fails with `Closed`, so closing
    /// `rings[..close()]` closes every producer's ring.
    pub fn close(&self) -> usize {
        self.state.fetch_or_release(CLOSED) & !CLOSED
    }

    /// The number of claimed ids. Never above the `max` passed to `claim`
    /// (INV-CH-04), so it is always a valid bound for slicing the rings.
    /// Relaxed: it synchronizes with nothing; use `count_acquire` for that.
    #[inline]
    pub fn count(&self) -> usize {
        self.state.load_relaxed() & !CLOSED
    }

    /// `count`, with Acquire: a thread that sees `k` here sees everything the
    /// first `k` registering threads did before their `claim`.
    #[inline]
    pub fn count_acquire(&self) -> usize {
        self.state.load_acquire() & !CLOSED
    }

    /// Whether `close` has run. The channels report `is_closed()` from their
    /// own flag, set after their rings close, so only the tests use this.
    #[cfg(any(test, feature = "loom"))]
    #[inline]
    pub fn is_closed(&self) -> bool {
        self.state.load_acquire() & CLOSED != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_up_to_max_then_full() {
        let r = Registry::new();
        assert_eq!(r.claim(2), Ok(0));
        assert_eq!(r.claim(2), Ok(1));
        assert_eq!(r.claim(2), Err(ClaimError::Full));
        assert_eq!(r.count(), 2);
    }

    #[test]
    fn close_reports_count_and_blocks_claims() {
        let r = Registry::new();
        assert_eq!(r.claim(4), Ok(0));
        assert!(!r.is_closed());
        assert_eq!(r.close(), 1);
        assert!(r.is_closed());
        assert_eq!(r.claim(4), Err(ClaimError::Closed));
        assert_eq!(r.count(), 1);
        // Closing twice is harmless and reports the same count.
        assert_eq!(r.close(), 1);
    }

    #[test]
    fn closed_wins_over_full() {
        let r = Registry::new();
        assert_eq!(r.claim(1), Ok(0));
        r.close();
        assert_eq!(r.claim(1), Err(ClaimError::Closed));
    }
}
