//! Send gates: a lossless close for code that sends through producers
//! (INV-CH-05).
//!
//! Closing a channel stops new reserves, but a send that reserved just
//! before the close can still commit just after it. A consumer that does its
//! final drain in between misses that item, although the send succeeded.
//! Wrappers that report "sent" to their callers (`ringmpsc-stream`,
//! `span_collector`) need to rule that out.
//!
//! A `SendGates` has one word per producer id, each on its own cache line.
//! A send opens its producer's gate (a *section*) before reserving and ends
//! it after committing; `close` sets a closed bit in every gate; and the
//! consumer does its final drain only once every gate shows the bit set and
//! no section open.
//!
//! Each send touches only its own producer's cache line, which no other
//! producer writes, so producers don't contend (the point of ring
//! decomposition). The only other writer is `close`, once.
//!
//! **Why it works.** For one gate, the sender's open (`compare_exchange`
//! from 0) and end (`fetch_and`), and the closer's `fetch_or`, are all
//! read-modify-writes on one word, so they fall in one order:
//! - the open comes after the close: it sees the bit and fails, so nothing
//!   is sent;
//! - the open comes before the close: the gate shows a section until the end,
//!   whose Release publishes the commit. The consumer's Acquire load that
//!   sees the gate back at `CLOSED` reads from a release sequence headed by
//!   that end, so its drain afterwards sees the item.

use crate::invariants::{debug_assert_gate_not_reentered, debug_assert_section_was_open};
use crate::registry::AtomicWord;
use crate::ring::CacheAligned;

/// Set by `close` in every gate.
const CLOSED: usize = 1 << (usize::BITS - 1);
/// Set while the gate's producer is between its open and its end.
const IN_SECTION: usize = 1;

/// The gates over any atomic word; [`SendGates`] is the std version. Exported
/// only for the loom tests, so they run this exact code.
pub struct GatesImpl<W: AtomicWord> {
    gates: Box<[CacheAligned<W>]>,
}

impl<W: AtomicWord> GatesImpl<W> {
    /// One gate per word, each starting at 0 (open, no section).
    pub fn from_words(words: impl IntoIterator<Item = W>) -> Self {
        Self {
            gates: words.into_iter().map(CacheAligned::new).collect(),
        }
    }

    /// Opens producer `id`'s section, or returns `None` if the gates are
    /// closed or `id` has no gate (the same in every build profile). Hold at
    /// most one section per id, and end it with [`SectionImpl::end`] after
    /// the commit.
    pub fn begin(&self, id: usize) -> Option<SectionImpl<'_, W>> {
        let gate: &W = self.gates.get(id)?;
        // Only this producer sets IN_SECTION, and only `close` sets CLOSED,
        // so a failure means the gate is closed. Success needs no ordering:
        // it reads the initial 0 or this producer's own earlier end, and
        // pairs with nothing. Failure is Acquire, pairing with `close`'s
        // Release.
        match gate.cas_relaxed_acquire(0, IN_SECTION) {
            Ok(_) => Some(SectionImpl { gate, ended: false }),
            Err(_prev) => {
                // A section already open for this id would fail here too and
                // look like a close; catch that misuse in debug builds.
                debug_assert_gate_not_reentered!(_prev, CLOSED);
                None
            }
        }
    }

    /// `true` if producer `id`'s gate is not closed yet (Acquire). `false`
    /// once `close` has reached it, or if `id` has no gate.
    pub fn is_open(&self, id: usize) -> bool {
        self.gates
            .get(id)
            .is_some_and(|gate| gate.load_acquire() & CLOSED == 0)
    }

    /// Closes every gate: each later `begin` fails. Sections already open
    /// stay open until they end.
    pub fn close(&self) {
        for gate in &*self.gates {
            gate.fetch_or_release(CLOSED);
        }
    }

    /// `true` once every gate is closed with no section open. Then every
    /// send that got a section has committed, and those commits are visible
    /// to this thread (Acquire), so a drain after this sees them all.
    pub fn is_quiet(&self) -> bool {
        self.gates.iter().all(|gate| gate.load_acquire() == CLOSED)
    }
}

/// An open section on one gate. End it with [`end`](Self::end); dropping it
/// also ends it (for example on a panic), but then the caller can't learn
/// whether to wake a waiting consumer.
pub struct SectionImpl<'a, W: AtomicWord> {
    gate: &'a W,
    ended: bool,
}

impl<W: AtomicWord> SectionImpl<'_, W> {
    /// Ends the section and returns `true` if the gates were closed while
    /// it was open. The caller should then wake a consumer that may be
    /// waiting in [`GatesImpl::is_quiet`].
    pub fn end(mut self) -> bool {
        self.ended = true;
        self.release()
    }

    fn release(&self) -> bool {
        // Release: the commit made in the section happens-before a consumer
        // whose Acquire load sees the gate back at CLOSED (see module docs).
        let prev = self.gate.fetch_and_release(!IN_SECTION);
        debug_assert_section_was_open!(prev & IN_SECTION);
        prev & CLOSED != 0
    }
}

impl<W: AtomicWord> Drop for SectionImpl<'_, W> {
    fn drop(&mut self) {
        if !self.ended {
            self.release();
        }
    }
}

/// One gate per producer id, for a lossless close (INV-CH-05).
///
/// For code that sends through [`Producer`](crate::Producer)s and reports
/// success to its own callers. Open a section with [`begin`](Self::begin)
/// before reserving, and end it after committing. After
/// [`close`](Self::close), do the final drain only once
/// [`is_quiet`](Self::is_quiet) returns `true`: then every send that got a
/// section has committed. Sections must be short and non-blocking (a reserve
/// and a commit, never a wait for space), because the consumer waits for
/// them.
///
/// ```
/// use ringmpsc_rs::{Channel, Config, SendGates};
///
/// let channel = Channel::<u64>::new(Config::new(3, 1, false));
/// let gates = SendGates::new(1);
/// let producer = channel.register().expect("one producer");
///
/// if let Some(section) = gates.begin(producer.id()) {
///     assert!(producer.push(7));
///     let _closed_meanwhile = section.end();
/// }
///
/// gates.close();
/// channel.close();
/// assert!(gates.begin(producer.id()).is_none());
/// assert!(gates.is_quiet()); // no section open: safe to drain for the last time
/// ```
pub struct SendGates(GatesImpl<std::sync::atomic::AtomicUsize>);

/// An open section from [`SendGates::begin`].
pub struct SendSection<'a>(SectionImpl<'a, std::sync::atomic::AtomicUsize>);

impl SendGates {
    /// Gates for producer ids `0..producers`, all open.
    #[must_use]
    pub fn new(producers: usize) -> Self {
        Self(GatesImpl::from_words(
            (0..producers).map(|_| std::sync::atomic::AtomicUsize::new(0)),
        ))
    }

    /// Opens a section for producer `id` before it reserves, or returns
    /// `None` once the gates are closed. At most one section per id at a
    /// time.
    ///
    /// Also returns `None` if `id` has no gate (`id >= producers`), in every
    /// build profile. That is a caller bug, and the sender then behaves as
    /// if closed, so size the gates to the channel's `max_producers`.
    #[inline]
    #[must_use]
    pub fn begin(&self, id: usize) -> Option<SendSection<'_>> {
        self.0.begin(id).map(SendSection)
    }

    /// `true` if producer `id`'s gate is not closed yet. Useful for a
    /// registration that must not hand out a sender whose gate is already
    /// closed.
    #[inline]
    #[must_use]
    pub fn is_open(&self, id: usize) -> bool {
        self.0.is_open(id)
    }

    /// Closes every gate. Call it before (or with) closing the channel.
    pub fn close(&self) {
        self.0.close();
    }

    /// `true` once closed and no section is open, so a drain now sees every
    /// item a section committed.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        self.0.is_quiet()
    }
}

impl std::fmt::Debug for SendGates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendGates")
            .field("producers", &self.0.gates.len())
            .field("quiet", &self.0.is_quiet())
            .finish()
    }
}

impl SendSection<'_> {
    /// Ends the section after the commit. Returns `true` if the gates were
    /// closed meanwhile, so the caller should wake a consumer that may be
    /// waiting for quiet.
    #[inline]
    #[must_use = "if this returns true, wake the consumer waiting for quiet"]
    pub fn end(self) -> bool {
        self.0.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_section_then_close_is_not_quiet_until_it_ends() {
        let gates = SendGates::new(2);
        let section = gates.begin(1).expect("open");
        gates.close();
        assert!(gates.begin(0).is_none(), "closed gates refuse new sections");
        assert!(!gates.is_quiet(), "a section is still open");
        assert!(section.end(), "the close happened while it was open");
        assert!(gates.is_quiet());
    }

    #[test]
    fn section_before_close_reports_not_closed() {
        let gates = SendGates::new(1);
        let section = gates.begin(0).expect("open");
        assert!(!section.end());
        assert!(!gates.is_quiet(), "not closed yet");
        gates.close();
        assert!(gates.is_quiet());
    }

    #[test]
    fn dropped_section_still_ends() {
        let gates = SendGates::new(1);
        drop(gates.begin(0).expect("open"));
        gates.close();
        assert!(gates.is_quiet());
    }

    /// A second section for one id is misuse; debug builds catch it instead
    /// of reporting a spurious close.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "INV-CH-05 violated: a second send section")]
    fn second_section_for_one_id_is_caught() {
        let gates = SendGates::new(1);
        let _first = gates.begin(0).expect("open");
        let _second = gates.begin(0);
    }

    #[test]
    fn unknown_id_gets_no_section() {
        let gates = SendGates::new(1);
        // The same in every build profile.
        assert!(gates.begin(5).is_none());
        assert!(!gates.is_open(5));
    }

    #[test]
    fn is_open_until_close() {
        let gates = SendGates::new(2);
        assert!(gates.is_open(0) && gates.is_open(1));
        gates.close();
        assert!(!gates.is_open(0) && !gates.is_open(1));
    }
}
