use crate::Ring;
use std::mem::MaybeUninit;

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
    /// You can commit fewer items than reserved by passing a count < len().
    pub fn commit(self) {
        let len = self.len;
        self.commit_n(len);
    }

    /// Commits n items (where n <= len()).
    ///
    /// # Panics
    ///
    /// Panics if `n` is greater than the number of reserved slots.
    pub fn commit_n(self, n: usize) {
        assert!(n <= self.len, "Cannot commit more than reserved");
        unsafe {
            let ring = &*self.ring_ptr;
            ring.commit_internal(n);
        }
    }
}
