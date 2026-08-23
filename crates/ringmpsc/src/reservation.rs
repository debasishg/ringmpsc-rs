use crate::allocator::{BufferAllocator, HeapAllocator};
use crate::Ring;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ptr::NonNull;
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
pub struct Reservation<'a, T, A: BufferAllocator = HeapAllocator> {
    /// Mutable slice into the ring buffer for writing data.
    slice: &'a mut [MaybeUninit<T>],

    /// Raw pointer to the parent Ring for commit operations.
    ///
    /// # Safety Invariant
    ///
    /// This raw pointer is safe to dereference for the lifetime `'a` because:
    ///
    /// 1. **Lifetime Coupling**: The `slice` field borrows from the Ring's buffer
    ///    with lifetime `'a`. Since `slice` keeps the Ring borrowed, the Ring
    ///    cannot be dropped or moved while this Reservation exists.
    ///
    /// 2. **Single Producer**: Each Ring has exactly one Producer (SPSC design).
    ///    The Producer creates Reservations and holds an `Arc<Ring<T, A>>`, ensuring
    ///    the Ring outlives any Reservation it creates.
    ///
    /// 3. **No Aliasing Violations**: We only use the pointer to call
    ///    `commit_internal()`, which accesses atomic fields that are safe to
    ///    access through a shared reference.
    ///
    /// We use a raw pointer instead of `&'a Ring<T, A>` to avoid borrow checker
    /// complications when the slice already borrows from the Ring's buffer.
    ///
    /// `NonNull` rather than `*const`: the pointer is always derived from a
    /// reference in `make_reservation`, so non-null is an invariant the type
    /// should state rather than leave to a comment. Making it structural
    /// deleted the `debug_assert_valid_ring_ptr!` null check that used to run
    /// in `commit_n_unchecked` on every commit in debug builds.
    ///
    /// Note this buys no size win: `Option<Reservation<..>>` was already
    /// niche-optimized on the `slice` reference, and stays 32 bytes either way
    /// (see `size_tests` below).
    ring: NonNull<Ring<T, A>>,

    /// Number of slots reserved (cached from `slice.len()`).
    len: usize,

    /// Records the logical `&'a mut` borrow of the ring.
    ///
    /// Redundant-looking next to `slice`, which already carries `'a`, but it
    /// does two things `slice` does not:
    ///
    /// 1. **Survives refactors.** If `slice` were ever replaced by a raw
    ///    pointer plus a length, `'a` would become unused and this struct
    ///    would stop compiling. This keeps the lifetime load-bearing
    ///    regardless of how the payload is stored.
    /// 2. **Fixes variance in `A`.** `slice` forces invariance in `T`, but
    ///    nothing else here constrains `A` - `NonNull<Ring<T, A>>` is
    ///    covariant in it. A reservation belongs to one specific ring with one
    ///    specific allocator, and this says so.
    _borrow: PhantomData<&'a mut Ring<T, A>>,
}

impl<'a, T, A: BufferAllocator> Reservation<'a, T, A> {
    /// Creates a new reservation.
    ///
    /// # Panics
    ///
    /// Panics if `ring_ptr` is null. Callers derive it from `&self`, so this
    /// is unreachable in practice; the check exists so the `NonNull` invariant
    /// is established in one place rather than assumed at each call site.
    pub(crate) fn new(slice: &'a mut [MaybeUninit<T>], ring_ptr: *const Ring<T, A>) -> Self {
        let len = slice.len();
        let ring = NonNull::new(ring_ptr.cast_mut()).expect("ring pointer is never null");
        Self {
            slice,
            ring,
            len,
            _borrow: PhantomData,
        }
    }

    /// Returns a mutable slice for writing data.
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [MaybeUninit<T>] {
        self.slice
    }

    /// Returns the number of reserved slots.
    #[inline]
    #[must_use] 
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns true if the reservation is empty.
    #[inline]
    #[must_use] 
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

    /// Commits exactly n items (where n <= `len()`).
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
        // INV-RES-03: pointer validity is enforced by the `NonNull` field type.
        // SAFETY: `ring` is non-null by construction and valid for `'a`, which
        // outlives `self`.
        let ring = unsafe { self.ring.as_ref() };
        ring.commit_internal(n);
    }

    /// Commits n items, saturating at `len()` if n is too large.
    ///
    /// This never fails - if you request more than available, it commits
    /// all available items.
    ///
    /// Returns the number of items actually committed.
    #[must_use] 
    pub fn commit_up_to(self, n: usize) -> usize {
        let to_commit = n.min(self.len);
        // SAFETY: to_commit <= self.len by construction
        unsafe { self.commit_n_unchecked(to_commit) };
        to_commit
    }
}

#[cfg(test)]
mod size_tests {
    use super::*;

    /// `reserve` returns `Option<Reservation<..>>` on the hot path, so the
    /// option must not cost a discriminant word.
    ///
    /// This is a regression guard, not a claim about `NonNull`: the niche comes
    /// from the `slice` reference and was already present before `ring` became
    /// `NonNull`. It would only break if the payload stopped being a reference.
    #[test]
    fn option_reservation_is_niche_optimized() {
        type R<'a> = Reservation<'a, u64, HeapAllocator>;
        assert_eq!(
            std::mem::size_of::<Option<R<'static>>>(),
            std::mem::size_of::<R<'static>>(),
            "Option<Reservation> should use a pointer niche, not a discriminant"
        );
    }
}
