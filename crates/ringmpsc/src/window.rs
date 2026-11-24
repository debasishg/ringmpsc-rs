//! Pure index and window arithmetic for `Ring` and `StackRing`.
//!
//! Every buffer index and every slice length the rings hand to `unsafe` code
//! is computed here. The functions are safe, pure, and free of generics,
//! pointers and atomics, so they can be translated to Lean by Charon and
//! Aeneas and proved for all inputs (see `RINGLEAN_DESIGN.md`, Phase 0).
//!
//! The functions are written for the translation:
//! - `if`/`else` instead of `min` and `saturating_sub`, so the Lean code
//!   needs no trait-method models
//! - no early returns, no loops
//!
//! # Contract
//!
//! Each function states its preconditions under `# Requires`. They are
//! facts the callers already maintain:
//! - `cap` is a power of two, `cap ≥ 1` (INV-MEM-02)
//! - `mask == cap - 1`
//! - `tail - head ≤ cap`, in wrapping `u64` arithmetic (INV-SEQ-01)
//!
//! The Lean theorems for each function are listed under `# Proves`.

/// Buffer index of the slot holding sequence number `seq`.
///
/// # Requires
/// `mask == cap - 1` with `cap` a power of two.
///
/// # Proves
/// `slot_index(seq, mask) < cap`.
#[inline]
#[must_use]
pub(crate) const fn slot_index(seq: u64, mask: usize) -> usize {
    (seq as usize) & mask
}

/// Number of items between `head` and `tail`: `tail - head` in wrapping
/// arithmetic.
///
/// # Requires
/// `tail - head ≤ cap` (INV-SEQ-01), so the result is exact.
#[inline]
#[must_use]
pub(crate) const fn occupancy(tail: u64, head: u64) -> usize {
    tail.wrapping_sub(head) as usize
}

/// Free slots the producer may reserve, given its view of `head`.
///
/// `head` may be the producer's cached copy, which can lag the real `head`.
/// A stale `head` only makes the result smaller, never larger.
///
/// # Requires
/// `cap ≥ 1`.
///
/// # Proves
/// - `free_space(tail, head, cap) ≤ cap`
/// - if `tail - head ≤ cap`: `occupancy(tail, head) + free_space(..) == cap`,
///   so a reservation of `n ≤ free_space` slots at `tail` does not reach `head + cap`
#[inline]
#[must_use]
// Written as `if`/`else`, not `saturating_sub`, so Aeneas needs no model of it.
#[allow(clippy::implicit_saturating_sub)]
pub(crate) const fn free_space(tail: u64, head: u64, cap: usize) -> usize {
    let used = occupancy(tail, head);
    if used >= cap {
        0
    } else {
        cap - used
    }
}

/// Buffer window `(idx, len)` for a reservation of up to `n` slots at `tail`.
///
/// `len` may be less than `n` when the reservation would cross the end of
/// the buffer (the partial-reservation rule, INV-RES-01).
///
/// # Requires
/// `cap` a power of two.
///
/// # Proves
/// - `idx < cap`
/// - `idx + len ≤ cap`
/// - `len ≤ n`
/// - `n ≥ 1 → len ≥ 1`
#[inline]
#[must_use]
pub(crate) const fn write_window(tail: u64, n: usize, cap: usize) -> (usize, usize) {
    let idx = slot_index(tail, cap - 1);
    let room = cap - idx;
    let len = if n < room { n } else { room };
    (idx, len)
}

/// Buffer window `(idx, len)` of the readable items at `head`, given the
/// consumer's view of `tail`. Returns `None` when nothing is readable.
///
/// `tail` may be the consumer's cached copy, which can lag the real `tail`
/// and can even trail `head` after a batch consume (#17). Comparing instead
/// of subtracting makes a trailing cache read as empty, not as a huge count.
///
/// # Requires
/// `cap` a power of two.
///
/// # Proves
/// - `idx < cap`
/// - `idx + len ≤ cap`
/// - `1 ≤ len ≤ tail - head`, so the window lies in `[head, tail)` (INV-INIT-01)
/// - `tail ≤ head → None`
#[inline]
#[must_use]
pub(crate) const fn read_window(head: u64, tail: u64, cap: usize) -> Option<(usize, usize)> {
    if tail <= head {
        None
    } else {
        let avail = (tail - head) as usize;
        let idx = slot_index(head, cap - 1);
        let room = cap - idx;
        let len = if avail < room { avail } else { room };
        Some((idx, len))
    }
}

/// Number of items a bounded consume takes: `min(avail, max)`.
///
/// # Proves
/// `consume_count(avail, max) ≤ avail` and `≤ max`.
#[inline]
#[must_use]
pub(crate) const fn consume_count(avail: usize, max: usize) -> usize {
    if avail < max {
        avail
    } else {
        max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPS: [usize; 6] = [1, 2, 4, 8, 16, 64];

    /// Sequence numbers around 0, around the capacity, and around the u64
    /// wrap point.
    fn seqs(cap: usize) -> Vec<u64> {
        let c = cap as u64;
        let mut v = Vec::new();
        for base in [0, c, 3 * c + 1, u64::MAX - 2 * c, u64::MAX - c, u64::MAX] {
            for d in 0..=2 * c {
                v.push(base.wrapping_add(d));
            }
        }
        v
    }

    #[test]
    fn slot_index_in_bounds() {
        for cap in CAPS {
            for seq in seqs(cap) {
                let idx = slot_index(seq, cap - 1);
                assert!(idx < cap);
                assert_eq!(idx, (seq % cap as u64) as usize);
            }
        }
    }

    #[test]
    fn free_space_complements_occupancy() {
        for cap in CAPS {
            for head in seqs(cap) {
                for used in 0..=cap as u64 {
                    let tail = head.wrapping_add(used);
                    let free = free_space(tail, head, cap);
                    assert!(free <= cap);
                    assert_eq!(occupancy(tail, head) + free, cap);
                }
            }
        }
    }

    #[test]
    fn free_space_stale_head_is_conservative() {
        for cap in CAPS {
            for head in seqs(cap) {
                for used in 0..=cap as u64 {
                    let tail = head.wrapping_add(used);
                    for lag in 0..=used {
                        let stale = head.wrapping_sub(lag);
                        assert!(free_space(tail, stale, cap) <= free_space(tail, head, cap));
                    }
                }
            }
        }
    }

    #[test]
    fn write_window_in_bounds() {
        for cap in CAPS {
            for tail in seqs(cap) {
                for n in 0..=cap + 1 {
                    let (idx, len) = write_window(tail, n, cap);
                    assert!(idx < cap);
                    assert!(idx + len <= cap);
                    assert!(len <= n);
                    if n >= 1 {
                        assert!(len >= 1);
                    }
                }
            }
        }
    }

    #[test]
    fn read_window_in_bounds_and_published() {
        for cap in CAPS {
            for head in seqs(cap) {
                for d in 0..=cap as u64 {
                    let tail = head.wrapping_add(d);
                    match read_window(head, tail, cap) {
                        None => assert!(tail <= head),
                        Some((idx, len)) => {
                            assert!(tail > head);
                            assert!(idx < cap);
                            assert!(idx + len <= cap);
                            assert!(len >= 1);
                            assert!(len as u64 <= tail - head);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn read_window_trailing_cache_is_empty() {
        // #17: a cached tail behind head must read as empty.
        for cap in CAPS {
            for head in seqs(cap) {
                if head == 0 {
                    continue;
                }
                assert_eq!(read_window(head, head - 1, cap), None);
                assert_eq!(read_window(head, head, cap), None);
            }
        }
    }

    #[test]
    fn consume_count_is_min() {
        for avail in 0..10 {
            for max in 0..10 {
                assert_eq!(consume_count(avail, max), avail.min(max));
            }
        }
    }
}
