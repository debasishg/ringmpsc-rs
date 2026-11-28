//! Loom-based concurrency tests for ringmpsc-rs.
//!
//! Run with: `cargo test --features loom --test loom_tests --release`
//!
//! Loom exhaustively explores all possible thread interleavings to find
//! concurrency bugs that might only occur under specific scheduling.

#![cfg(feature = "loom")]

use loom::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use loom::sync::Arc;
use loom::thread;
use std::cell::UnsafeCell;

/// Simplified ring buffer for loom testing.
/// 
/// We test the core synchronization protocol in isolation, using a smaller
/// capacity to keep the state space manageable for loom's exhaustive search.
struct LoomRing {
    /// Tail index (written by producer)
    tail: AtomicU64,
    /// Head index (written by consumer)  
    head: AtomicU64,
    /// Buffer (simplified to just track writes)
    buffer: UnsafeCell<[u64; 4]>,
    capacity: usize,
}

unsafe impl Send for LoomRing {}
unsafe impl Sync for LoomRing {}

impl LoomRing {
    fn new() -> Self {
        Self::with_capacity(4)
    }

    /// A ring using the first `capacity` slots of the buffer. A smaller ring
    /// fills up in fewer steps, which keeps loom's search small.
    fn with_capacity(capacity: usize) -> Self {
        assert!(capacity.is_power_of_two() && capacity <= 4);
        Self {
            tail: AtomicU64::new(0),
            head: AtomicU64::new(0),
            buffer: UnsafeCell::new([0; 4]),
            capacity,
        }
    }

    /// `len()` as `Ring::len` computes it: the same loads and orderings as
    /// `occupancy_snapshot` in `src/ring.rs`. Keep the two in sync.
    fn len(&self) -> u64 {
        loop {
            let h1 = self.head.load(Ordering::Acquire);
            let t = self.tail.load(Ordering::Acquire);
            let h2 = self.head.load(Ordering::Relaxed);
            if h1 == h2 {
                return t.wrapping_sub(h1);
            }
        }
    }

    fn mask(&self) -> usize {
        self.capacity - 1
    }

    /// Producer: try to push a value
    fn push(&self, value: u64) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        
        let space = self.capacity.saturating_sub((tail - head) as usize);
        if space == 0 {
            return false;
        }

        let idx = (tail as usize) & self.mask();
        
        // SAFETY: We verified space > 0, so this slot is available
        unsafe {
            (*self.buffer.get())[idx] = value;
        }
        
        // Release: publishes the write to consumer
        self.tail.store(tail + 1, Ordering::Release);
        true
    }

    /// Consumer: try to pop a value
    fn pop(&self) -> Option<u64> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        
        if head == tail {
            return None;
        }

        let idx = (head as usize) & self.mask();
        
        // SAFETY: We verified tail > head, so this slot has data
        let value = unsafe { (*self.buffer.get())[idx] };
        
        // Release: publishes consumption to producer
        self.head.store(head + 1, Ordering::Release);
        Some(value)
    }
}

/// Test basic SPSC push/pop with loom's exhaustive interleaving exploration.
#[test]
fn loom_spsc_basic() {
    loom::model(|| {
        let ring = Arc::new(LoomRing::new());
        let ring2 = Arc::clone(&ring);

        // Producer thread
        let producer = thread::spawn(move || {
            ring2.push(42);
            ring2.push(43);
        });

        // Consumer thread
        let consumer = thread::spawn(move || {
            let mut received = Vec::new();
            // Try multiple times since producer might not have pushed yet
            for _ in 0..10 {
                if let Some(v) = ring.pop() {
                    received.push(v);
                }
                if received.len() == 2 {
                    break;
                }
                loom::thread::yield_now();
            }
            received
        });

        producer.join().unwrap();
        let received = consumer.join().unwrap();
        
        // Verify FIFO order if we received anything
        if received.len() >= 2 {
            assert_eq!(received[0], 42);
            assert_eq!(received[1], 43);
        }
    });
}

/// Test that producer blocks when ring is full.
#[test]
fn loom_spsc_full_ring() {
    loom::model(|| {
        let ring = Arc::new(LoomRing::new());
        let ring2 = Arc::clone(&ring);

        // Fill the ring (capacity = 4)
        assert!(ring.push(1));
        assert!(ring.push(2));
        assert!(ring.push(3));
        assert!(ring.push(4));
        
        // Should fail - ring is full
        assert!(!ring.push(5));

        // Consumer frees one slot
        let consumer = thread::spawn(move || {
            ring2.pop()
        });

        let value = consumer.join().unwrap();
        assert_eq!(value, Some(1));
        
        // Now producer can push
        assert!(ring.push(5));
    });
}

/// Test concurrent producer and consumer with multiple items.
#[test]
fn loom_spsc_concurrent() {
    loom::model(|| {
        let ring = Arc::new(LoomRing::new());
        let ring_producer = Arc::clone(&ring);
        let ring_consumer = Arc::clone(&ring);

        let sent = Arc::new(AtomicUsize::new(0));
        let received = Arc::new(AtomicUsize::new(0));
        
        let sent_clone = Arc::clone(&sent);
        let received_clone = Arc::clone(&received);

        // Producer: send 2 items
        let producer = thread::spawn(move || {
            if ring_producer.push(100) {
                sent_clone.fetch_add(1, Ordering::SeqCst);
            }
            if ring_producer.push(200) {
                sent_clone.fetch_add(1, Ordering::SeqCst);
            }
        });

        // Consumer: receive items
        let consumer = thread::spawn(move || {
            for _ in 0..4 {
                if ring_consumer.pop().is_some() {
                    received_clone.fetch_add(1, Ordering::SeqCst);
                }
                loom::thread::yield_now();
            }
        });

        producer.join().unwrap();
        consumer.join().unwrap();

        // Received should not exceed sent
        let s = sent.load(Ordering::SeqCst);
        let r = received.load(Ordering::SeqCst);
        assert!(r <= s, "received {} but only sent {}", r, s);
    });
}

/// Test the cached sequence number optimization pattern.
/// This verifies our fast-path/slow-path cache refresh is correct.
#[test]
fn loom_cached_sequence_pattern() {
    loom::model(|| {
        let tail = Arc::new(AtomicU64::new(0));
        let head = Arc::new(AtomicU64::new(0));
        
        // Simulated cached values (would be UnsafeCell in real code)
        let cached_head = Arc::new(AtomicU64::new(0));
        let cached_tail = Arc::new(AtomicU64::new(0));

        let tail_p = Arc::clone(&tail);
        let head_p = Arc::clone(&head);
        let cached_head_p = Arc::clone(&cached_head);

        let tail_c = Arc::clone(&tail);
        let head_c = Arc::clone(&head);
        let cached_tail_c = Arc::clone(&cached_tail);

        // Producer: uses cached_head, refreshes from head when needed
        let producer = thread::spawn(move || {
            let t = tail_p.load(Ordering::Relaxed);
            
            // Fast path: check cache
            let ch = cached_head_p.load(Ordering::Relaxed);
            let space = 4usize.saturating_sub((t.wrapping_sub(ch)) as usize);
            
            if space == 0 {
                // Slow path: refresh cache
                let h = head_p.load(Ordering::Acquire);
                cached_head_p.store(h, Ordering::Relaxed);
            }
            
            // Publish write
            tail_p.store(t + 1, Ordering::Release);
        });

        // Consumer: uses cached_tail, refreshes from tail when needed  
        let consumer = thread::spawn(move || {
            let h = head_c.load(Ordering::Relaxed);
            
            // Fast path: check cache
            let ct = cached_tail_c.load(Ordering::Relaxed);
            let avail = ct.wrapping_sub(h) as usize;
            
            if avail == 0 {
                // Slow path: refresh cache
                let t = tail_c.load(Ordering::Acquire);
                cached_tail_c.store(t, Ordering::Relaxed);
            }
            
            // Publish consumption
            head_c.store(h + 1, Ordering::Release);
        });

        producer.join().unwrap();
        consumer.join().unwrap();
        
        // Both should have advanced by 1
        assert_eq!(tail.load(Ordering::SeqCst), 1);
        assert_eq!(head.load(Ordering::SeqCst), 1);
    });
}

/// `len()` from a third thread, while a producer and a consumer both move.
///
/// A 2-slot ring with 3 items makes the producer wait for the consumer, so
/// `head` and `tail` both advance while the observer reads them. In every
/// explored interleaving, `len()` must be a consistent snapshot: never an old
/// `tail` with a newer `head` (wraps to about 2^64), never a new `tail` with an
/// old `head` (more than the capacity).
///
/// The observer is a spawned thread, not the model's main thread: with the
/// loads on the main thread, loom never interleaved them with the other two
/// threads, and the test passed even with the old, broken `len()`.
///
/// Preemptions are bounded at 2 so the test runs in seconds; unbounded, it
/// did not finish within 10 minutes. A bound of 1 already finds both bad
/// pairings in the old `len()`.
#[test]
fn loom_len_snapshot_from_observer() {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(2);
    builder.check(|| {
        const CAPACITY: u64 = 2;
        const ITEMS: u64 = 3;
        let ring = Arc::new(LoomRing::with_capacity(CAPACITY as usize));
        let producer_ring = Arc::clone(&ring);
        let consumer_ring = Arc::clone(&ring);

        let producer = thread::spawn(move || {
            let mut sent = 0;
            // Bounded retries keep loom's search finite.
            for _ in 0..8 {
                if sent == ITEMS {
                    break;
                }
                if producer_ring.push(sent) {
                    sent += 1;
                } else {
                    loom::thread::yield_now();
                }
            }
        });

        let consumer = thread::spawn(move || {
            let mut received = 0;
            for _ in 0..8 {
                if received == ITEMS {
                    break;
                }
                if consumer_ring.pop().is_some() {
                    received += 1;
                } else {
                    loom::thread::yield_now();
                }
            }
        });

        let observer = thread::spawn(move || {
            for _ in 0..2 {
                let len = ring.len();
                assert!(len <= CAPACITY, "len() = {len} > capacity {CAPACITY}");
            }
        });

        observer.join().expect("observer: len() out of range");
        producer.join().expect("producer panicked");
        consumer.join().expect("consumer panicked");
    });
}

// =============================================================================
// Registration vs close (#45, INV-CH-04)
//
// These run the real `Registry` from `src/registry.rs` over loom's atomic,
// not a copy, so a change to its orderings or its CAS loop is what loom sees.
// =============================================================================

type LoomRegistry = ringmpsc_rs::Registry<AtomicUsize>;

fn loom_registry() -> LoomRegistry {
    ringmpsc_rs::Registry::from_word(AtomicUsize::new(0))
}

/// A `register()` racing `close()` either gets an id that `close()` counted,
/// so its ring is closed, or fails with `Closed`. It never gets an id that
/// `close()` missed (bug 3 of #45).
#[test]
fn loom_register_vs_close() {
    loom::model(|| {
        let registry = Arc::new(loom_registry());
        let r = Arc::clone(&registry);
        let registrar = thread::spawn(move || r.claim(2));

        let closed_count = registry.close();
        let claimed = registrar.join().expect("registrar thread");

        match claimed {
            Ok(id) => assert!(
                id < closed_count,
                "id {id} registered but close() only saw {closed_count}"
            ),
            Err(e) => assert_eq!(e, ringmpsc_rs::ClaimError::Closed),
        }
        assert!(registry.is_closed());
        assert_eq!(registry.claim(2), Err(ringmpsc_rs::ClaimError::Closed));
    });
}

/// Two `register()`s race for the last slot while a reader checks the count.
/// The count never passes the limit, not even for an instant, so a reader
/// slicing `rings[..count]` stays in bounds (bug 2 of #45).
#[test]
fn loom_register_never_exceeds_max() {
    loom::model(|| {
        let registry = Arc::new(loom_registry());
        let claimers: Vec<_> = (0..2)
            .map(|_| {
                let r = Arc::clone(&registry);
                thread::spawn(move || r.claim(1))
            })
            .collect();

        for _ in 0..2 {
            let count = registry.count();
            assert!(count <= 1, "count {count} passed the limit 1");
        }

        let results: Vec<_> = claimers
            .into_iter()
            .map(|h| h.join().expect("claimer thread"))
            .collect();
        let won = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(won, 1, "exactly one claim gets the one slot: {results:?}");
        assert!(results.contains(&Ok(0)));
        assert!(results.contains(&Err(ringmpsc_rs::ClaimError::Full)));
        assert_eq!(registry.count(), 1);
    });
}

/// `producer_count()` synchronizes with `register()`: a thread that sees the
/// new count also sees what the registering thread wrote before registering.
/// The write is to a loom `UnsafeCell`, so loom reports a data race if the
/// claim's Release or the count's Acquire is missing.
#[test]
fn loom_producer_count_synchronizes_with_register() {
    loom::model(|| {
        let registry = Arc::new(loom_registry());
        let config = Arc::new(loom::cell::UnsafeCell::new(0u64));

        let (r, c) = (Arc::clone(&registry), Arc::clone(&config));
        let registrar = thread::spawn(move || {
            // Safety: only this thread writes, and only before its claim, which
            // publishes it (INV-CH-04: producer_count() synchronizes with
            // register()); loom checks the access.
            c.with_mut(|p| unsafe { *p = 42 });
            r.claim(1).expect("the only claim");
        });

        // Wait to see the registration, as a coordinator polling
        // `producer_count()` would. Without the wait, loom may never schedule
        // the read after the claim, and the test would check nothing.
        while registry.count_acquire() == 0 {
            thread::yield_now();
        }
        // Safety: (INV-CH-04) ordered after the write by the claim's Release and the
        // count's Acquire; loom checks that.
        let seen = config.with(|p| unsafe { *p });
        assert_eq!(seen, 42);
        registrar.join().expect("registrar thread");
    });
}

// =============================================================================
// Send gates (INV-CH-05), on the real `GatesImpl` over loom's AtomicUsize.
// =============================================================================

/// A send that opened its section before the close commits before the
/// consumer's final drain; one that opens after the close sends nothing.
/// The "commit" is a write to a loom `UnsafeCell`, and the final drain reads
/// it once the gates are quiet, so loom reports a race if the drain could run
/// before an open section's commit.
#[test]
fn loom_gate_section_before_close_is_drained() {
    loom::model(|| {
        let gates = Arc::new(ringmpsc_rs::GatesImpl::from_words([AtomicUsize::new(0)]));
        let slot = Arc::new(loom::cell::UnsafeCell::new(0u64));

        let (g, s) = (Arc::clone(&gates), Arc::clone(&slot));
        let sender = thread::spawn(move || {
            if let Some(section) = g.begin(0) {
                // Safety: (INV-CH-05) written only inside the section, which
                // the consumer waits for before reading.
                s.with_mut(|p| unsafe { *p = 42 });
                let _ = section.end();
                true
            } else {
                false
            }
        });

        gates.close();
        // Wait for quiet, as the consumer's final drain does.
        while !gates.is_quiet() {
            thread::yield_now();
        }
        // Safety: (INV-CH-05) every section that wrote has ended, and
        // `is_quiet()`'s Acquire orders that write before this read; loom
        // checks it.
        let seen = slot.with(|p| unsafe { *p });

        let sent = sender.join().expect("sender thread");
        assert_eq!(seen, if sent { 42 } else { 0 }, "a sent item was missed");
        assert!(gates.begin(0).is_none(), "closed gates refuse sections");
    });
}
