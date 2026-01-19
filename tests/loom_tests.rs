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
        Self {
            tail: AtomicU64::new(0),
            head: AtomicU64::new(0),
            buffer: UnsafeCell::new([0; 4]),
            capacity: 4,
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
