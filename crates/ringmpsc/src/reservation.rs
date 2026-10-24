use crate::allocator::{BufferAllocator, HeapAllocator};
use crate::Ring;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ptr::NonNull;
use thiserror::Error;

/// Error returned when trying to commit more items than were written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("cannot commit {attempted} items, only {available} written")]
pub struct CommitError {
    /// Number of items attempted to commit.
    pub attempted: usize,
    /// Number of items actually reserved.
    pub available: usize,
}

/// Zero-copy reservation for writing directly into the ring buffer.
///
/// The producer obtains a reservation, writes items into it in order with
/// [`push`](Self::push), [`extend`](Self::extend) or
/// [`extend_from_slice`](Self::extend_from_slice), then commits to make them
/// visible to the consumer. Writes go straight into the ring's slots; nothing
/// is copied again on commit.
///
/// The reservation **counts what was written**, and a commit publishes only
/// that prefix. Publishing a slot that was never written would hand the
/// consumer uninitialized memory (#32), so there is no way to do it.
///
/// Written items that a partial commit ([`commit_up_to`](Self::commit_up_to),
/// [`try_commit_n`](Self::try_commit_n)) does not publish are dropped. A
/// reservation dropped without committing publishes nothing; items written
/// into it are leaked, as before (safe, but not dropped).
///
/// **Important:** A `Reservation` may hold fewer slots than requested from
/// `reserve(n)` if the reservation wraps around the ring buffer boundary.
/// Check [`len`](Self::len) to see how many were actually reserved.
///
/// # Example
///
/// ```
/// use ringmpsc_rs::{Channel, Config};
///
/// let channel = Channel::<u64>::new(Config::default());
/// let mut producer = channel.register().unwrap();
///
/// // Request 100 slots; a wrap-around can grant fewer.
/// let mut reservation = producer.reserve(100).unwrap();
/// let granted = reservation.len();
/// let written = reservation.extend(0..);
/// assert_eq!(written, granted);
/// reservation.commit();
///
/// let mut seen = 0;
/// channel.consumer().unwrap().consume_all(|_| seen += 1);
/// assert_eq!(seen, granted);
/// ```
///
/// Committing without writing publishes nothing:
///
/// ```
/// use ringmpsc_rs::{Channel, Config};
///
/// let channel = Channel::<Box<u64>>::new(Config::default());
/// let mut producer = channel.register().unwrap();
/// producer.reserve(1).unwrap().commit();
/// assert_eq!(channel.consumer().unwrap().consume_all_owned(drop), 0);
/// ```
///
/// There is no safe access to the raw slots, because writes through it could
/// not be counted:
///
/// ```compile_fail,E0599
/// use ringmpsc_rs::{Channel, Config};
///
/// let channel = Channel::<u64>::new(Config::default());
/// let mut producer = channel.register().unwrap();
/// let mut reservation = producer.reserve(1).unwrap();
/// let _ = reservation.as_mut_slice(); // ERROR: no method `as_mut_slice`
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
    /// on every commit in debug builds.
    ///
    /// Note this buys no size win: `Option<Reservation<..>>` was already
    /// niche-optimized on the `slice` reference, and stays 32 bytes either way
    /// (see `size_tests` below).
    ring: NonNull<Ring<T, A>>,

    /// Number of slots reserved (cached from `slice.len()`).
    len: usize,

    /// Number of slots written, from the front: `slice[..filled]` are
    /// initialized. Only this prefix can be committed.
    filled: usize,

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
            filled: 0,
            _borrow: PhantomData,
        }
    }

    /// Returns the number of reserved slots.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns true if no slots were reserved.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns how many slots have been written.
    #[inline]
    #[must_use]
    pub fn filled(&self) -> usize {
        self.filled
    }

    /// Returns how many reserved slots are still unwritten.
    #[inline]
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.len - self.filled
    }

    /// Writes `value` into the next slot.
    ///
    /// Returns `Err(value)` if every reserved slot is already written.
    #[inline]
    pub fn push(&mut self, value: T) -> Result<(), T> {
        if self.filled == self.len {
            return Err(value);
        }
        self.slice[self.filled].write(value);
        self.filled += 1;
        Ok(())
    }

    /// Writes items from `iter` into the next slots, until the reservation
    /// is full or the iterator ends. Returns how many were written.
    ///
    /// Takes no more items from `iter` than there is room for.
    pub fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) -> usize {
        let start = self.filled;
        for value in iter.into_iter().take(self.len - self.filled) {
            self.slice[self.filled].write(value);
            self.filled += 1;
        }
        self.filled - start
    }

    /// Copies as many of `items` as fit into the next slots. Returns how many
    /// were written.
    pub fn extend_from_slice(&mut self, items: &[T]) -> usize
    where
        T: Copy,
    {
        let n = items.len().min(self.len - self.filled);
        for (slot, item) in self.slice[self.filled..self.filled + n].iter_mut().zip(items) {
            slot.write(*item);
        }
        self.filled += n;
        n
    }

    /// Commits every written item, making it visible to the consumer.
    /// Returns how many were committed.
    ///
    /// Unwritten slots are not published.
    pub fn commit(self) -> usize {
        let n = self.filled;
        self.commit_prefix(n);
        n
    }

    /// Commits exactly the first `n` written items.
    ///
    /// Returns `Ok(())` on success, or `Err(CommitError)` if fewer than `n`
    /// items were written. Written items past `n` are dropped.
    ///
    /// # Example
    ///
    /// ```
    /// use ringmpsc_rs::{Channel, Config};
    ///
    /// let channel = Channel::<u64>::new(Config::default());
    /// let mut producer = channel.register().unwrap();
    /// let mut reservation = producer.reserve(10).unwrap();
    /// reservation.extend(0..5);
    /// assert!(reservation.try_commit_n(3).is_ok()); // items 3 and 4 are dropped
    /// ```
    pub fn try_commit_n(self, n: usize) -> Result<(), CommitError> {
        if n > self.filled {
            return Err(CommitError {
                attempted: n,
                available: self.filled,
            });
        }
        self.commit_prefix(n);
        Ok(())
    }

    /// Commits up to `n` written items, saturating at [`filled`](Self::filled).
    /// Written items past `n` are dropped.
    ///
    /// Returns the number of items actually committed.
    #[must_use]
    pub fn commit_up_to(self, n: usize) -> usize {
        let to_commit = n.min(self.filled);
        self.commit_prefix(to_commit);
        to_commit
    }

    /// Publishes `slice[..n]`, which must be written (`n <= filled`), and
    /// drops the written items past it.
    fn commit_prefix(self, n: usize) {
        debug_assert!(n <= self.filled);
        let this = self;
        let filled = this.filled;
        for slot in &mut this.slice[n..filled] {
            // SAFETY: `slice[..filled]` is initialized (every write goes
            // through `push`/`extend`/`extend_from_slice`, which bump
            // `filled` after writing), and these slots are not published,
            // so this is the only owner of the items in them.
            unsafe { slot.assume_init_drop() };
        }
        if n == 0 {
            // Nothing to publish: no tail store, no metrics batch.
            return;
        }
        // INV-RES-03: pointer validity is enforced by the `NonNull` field type.
        // SAFETY: `ring` is non-null by construction and valid for `'a`, which
        // outlives `self`.
        let ring = unsafe { this.ring.as_ref() };
        // `slice[..n]` is initialized, so publishing it upholds INV-INIT-01.
        ring.commit_internal(n);
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
