use crate::Ring;
use std::mem::MaybeUninit;
use thiserror::Error;

/// Error returned when trying to commit more items than reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("cannot commit {attempted} items, only {available} reserved")]
pub struct CommitError {
    /// Number of items attempted to commit.
    pub attempted: usize,
    /// Number of items actually reserved.
    pub available: usize,
}

/// Zero-copy reservation for writing directly into the ring buffer.
///
/// The producer obtains a reservation, writes data into the provided slice,
/// then commits to make the data visible to the consumer.
///
/// **Important:** A `Reservation` may contain fewer items than requested from
/// `reserve(n)` if the reservation wraps around the ring buffer boundary. Always
/// check `as_mut_slice().len()` to determine how many items were actually reserved.
///
/// # Example
///
/// ```ignore
/// // Request 100 items but might get fewer
/// if let Some(mut reservation) = producer.reserve(100) {
///     let slice = reservation.as_mut_slice();
///     let actual = slice.len(); // May be < 100!
///     
///     // Write data to slice...
///     for item in slice.iter_mut() {
///         *item = some_value;
///     }
///     
///     reservation.commit(); // Commits `actual` items
/// }
/// ```
pub struct Reservation<'a, T> {
    slice: &'a mut [MaybeUninit<T>],
    ring_ptr: *const Ring<T>,
    len: usize,
}

impl<'a, T> Reservation<'a, T> {
    /// Creates a new reservation.
    pub(crate) fn new(slice: &'a mut [MaybeUninit<T>], ring_ptr: *const Ring<T>) -> Self {
        let len = slice.len();
        Self {
            slice,
            ring_ptr,
            len,
        }
    }

    /// Returns a mutable slice for writing data.
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [MaybeUninit<T>] {
        self.slice
    }

    /// Returns the number of reserved slots.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns true if the reservation is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Commits the reservation, making data visible to the consumer.
    ///
    /// This commits all reserved slots. Use `try_commit_n` if you want to
    /// commit fewer items than reserved.
    pub fn commit(self) {
        let len = self.len;
        // SAFETY: len is always <= self.len by construction
        unsafe { self.commit_n_unchecked(len) };
    }

    /// Commits exactly n items (where n <= len()).
    ///
    /// Returns `Ok(())` on success, or `Err(CommitError)` if `n > len()`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut reservation = producer.reserve(10).unwrap();
    /// // Only write 5 items...
    /// reservation.try_commit_n(5)?; // Commits only 5
    /// ```
    pub fn try_commit_n(self, n: usize) -> Result<(), CommitError> {
        if n > self.len {
            return Err(CommitError {
                attempted: n,
                available: self.len,
            });
        }
        // SAFETY: We just verified n <= self.len
        unsafe { self.commit_n_unchecked(n) };
        Ok(())
    }

    /// Commits n items without bounds checking.
    ///
    /// # Safety
    ///
    /// Caller must ensure `n <= self.len()`.
    #[inline]
    unsafe fn commit_n_unchecked(self, n: usize) {
        let ring = &*self.ring_ptr;
        ring.commit_internal(n);
    }

    /// Commits n items, saturating at len() if n is too large.
    ///
    /// This never fails - if you request more than available, it commits
    /// all available items.
    ///
    /// Returns the number of items actually committed.
    pub fn commit_up_to(self, n: usize) -> usize {
        let to_commit = n.min(self.len);
        // SAFETY: to_commit <= self.len by construction
        unsafe { self.commit_n_unchecked(to_commit) };
        to_commit
    }
}
