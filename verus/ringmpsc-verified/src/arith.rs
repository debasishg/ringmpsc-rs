//! Verified ring arithmetic.
//!
//! Each exec function here has the same body as the corresponding expression
//! in `crates/ringmpsc/src/ring.rs` and `config.rs`, plus a contract stating
//! what the ring relies on it for. Sequence numbers are unbounded `u64`s and
//! slots are `seq & mask`, so most of the ring's correctness reduces to these
//! few facts.

use vstd::prelude::*;

verus! {

// The crate targets 64-bit (see the workspace's cast lints), and `ring.rs`
// casts between `u64` sequence numbers and `usize` indices freely.
global size_of usize == 8;

/// Largest `ring_bits` that `Config::new` accepts.
pub const MAX_RING_BITS: u8 = 20;

/// The ring's capacity, as a mathematical integer.
pub open spec fn cap_spec(ring_bits: u8) -> int {
    (1u64 << ring_bits) as int
}

/// Capacity bounds for every `ring_bits` that `Config::new` accepts.
pub proof fn lemma_cap_bounds(b: u8)
    requires
        1 <= b <= MAX_RING_BITS,
    ensures
        2 <= (1u64 << b) <= 0x10_0000,
{
    assert(2 <= (1u64 << b) <= 0x10_0000) by (bit_vector)
        requires 1 <= b <= 20;
}

/// `Config::capacity`: `1 << ring_bits`.
pub fn capacity(ring_bits: u8) -> (cap: usize)
    requires
        1 <= ring_bits <= MAX_RING_BITS,
    ensures
        cap as int == cap_spec(ring_bits),
        2 <= cap <= 0x10_0000,
{
    proof {
        lemma_cap_bounds(ring_bits);
        // usize is 64 bits (see `global size_of` above).
        assert((1usize << ring_bits) as u64 == (1u64 << ring_bits)) by (bit_vector)
            requires ring_bits < 64;
    }
    1usize << ring_bits
}

/// For a power-of-two `cap`, masking with `cap - 1` is reduction mod `cap`.
/// This is what lets `ring.rs` index with `seq & mask` instead of `seq % cap`.
pub proof fn lemma_mask_is_mod(x: u64, b: u64)
    requires
        b < 64,
    ensures
        x & (((1u64 << b) - 1) as u64) == x % (1u64 << b),
{
    assert(x & (((1u64 << b) - 1) as u64) == x % (1u64 << b)) by (bit_vector)
        requires b < 64;
}

/// `ring.rs`: `(seq as usize) & mask`.
///
/// The slot for a sequence number is in bounds, and it is exactly
/// `seq mod capacity`, so sequence numbers `cap` apart share a slot and
/// consecutive ones don't.
pub fn slot_index(seq: u64, ring_bits: u8) -> (idx: usize)
    requires
        1 <= ring_bits <= MAX_RING_BITS,
    ensures
        (idx as int) == (seq as int) % cap_spec(ring_bits),
        (idx as int) < cap_spec(ring_bits),
{
    let cap = capacity(ring_bits);
    let mask = cap - 1;
    proof {
        assert((1u64 << ring_bits) == (1u64 << (ring_bits as u64))) by (bit_vector);
        lemma_mask_is_mod(seq, ring_bits as u64);
    }
    (seq as usize) & mask
}

/// Facts the ring maintains about its sequence numbers (INV-SEQ-01): head
/// never passes tail, and at most `cap` items are in flight.
pub open spec fn bounded(head: u64, tail: u64, cap: int) -> bool {
    head <= tail && tail - head <= cap
}

/// `Ring::len`: `tail.wrapping_sub(head) as usize`.
///
/// Under INV-SEQ-01 the wrapping subtraction never wraps, so this is the
/// exact number of items in the ring.
pub fn len(head: u64, tail: u64, ring_bits: u8) -> (n: usize)
    requires
        1 <= ring_bits <= MAX_RING_BITS,
        bounded(head, tail, cap_spec(ring_bits)),
    ensures
        n as int == tail - head,
        n as int <= cap_spec(ring_bits),
{
    tail.wrapping_sub(head) as usize
}

/// The free space a producer computes in `reserve_shared`:
/// `capacity.saturating_sub(tail.wrapping_sub(head) as usize)`.
pub fn space(head: u64, tail: u64, cap: usize) -> (free: usize)
    requires
        head <= tail,
    ensures
        tail - head <= cap ==> free as int == cap - (tail - head),
        tail - head > cap ==> free == 0,
{
    cap.saturating_sub(tail.wrapping_sub(head) as usize)
}

/// The producer's fast path computes space from `cached_head`, which may lag
/// the real `head` (the consumer only ever moves `head` forward). Space from a
/// stale head is never more than space from the real one, so if the fast
/// path finds room for `n`, the room is really there.
pub proof fn lemma_stale_head_underestimates_space(
    cached_head: u64,
    head: u64,
    tail: u64,
    cap: int,
    n: int,
)
    requires
        cached_head <= head,
        bounded(head, tail, cap),
        cached_head <= tail,
        tail - cached_head <= cap,
        cap - (tail - cached_head) >= n,
    ensures
        cap - (tail - head) >= n,
{
}

/// The consumer's fast path in `readable`: `cached_tail.wrapping_sub(head)`.
///
/// This is the item count only when `head <= cached_tail`, and nothing in
/// `ring.rs` maintains that. `consume_batch*` and `consume_up_to*` advance
/// `head` from a fresh load of `tail` without updating `cached_tail`, so a
/// later `readable` can see `cached_tail < head`. The subtraction then wraps
/// to a huge count, `readable` skips its refresh, and it returns a slice over
/// unwritten slots. Drop the `head <= cached_tail` precondition and this
/// function no longer verifies: that failure is the bug.
pub fn readable_avail(head: u64, cached_tail: u64) -> (avail: usize)
    requires
        head <= cached_tail,
    ensures
        avail as int == cached_tail - head,
{
    cached_tail.wrapping_sub(head) as usize
}

/// `make_reservation`: `n.min(capacity - idx)`.
///
/// A reservation never runs past the end of the buffer, never exceeds what
/// was asked for, and is non-empty whenever something was asked for.
pub fn contiguous(idx: usize, n: usize, cap: usize) -> (len: usize)
    requires
        idx < cap,
    ensures
        len <= n,
        idx + len <= cap,
        n > 0 ==> len > 0,
        len == n || idx + len == cap,
{
    n.min(cap - idx)
}

/// The slots of a contiguous reservation are consecutive buffer indices: for
/// `k < len`, sequence number `tail + k` lands on slot `idx + k`. So the
/// `&mut [MaybeUninit<T>]` that `make_reservation` builds from
/// `slot(idx)..slot(idx + len)` covers exactly the reserved sequence numbers.
pub proof fn lemma_contiguous_slots_do_not_wrap(tail: int, k: int, cap: int)
    requires
        cap > 0,
        tail >= 0,
        0 <= k,
        tail % cap + k < cap,
    ensures
        (tail + k) % cap == tail % cap + k,
{
    vstd::arithmetic::div_mod::lemma_fundamental_div_mod(tail, cap);
    vstd::arithmetic::div_mod::lemma_mod_multiples_vanish(tail / cap, tail % cap + k, cap);
    vstd::arithmetic::div_mod::lemma_small_mod((tail % cap + k) as nat, cap as nat);
}

} // verus!
