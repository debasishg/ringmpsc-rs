use crate::span::Span;
use ringmpsc_rs::{Backoff, Channel, ChannelError, Config, Consumer, SendGates};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;

/// Configuration for the span collector
#[derive(Debug, Clone)]
pub struct CollectorConfig {
    /// Ring buffer size as power of 2 (1-20)
    pub ring_bits: u8,
    /// Maximum number of producers
    pub max_producers: u8,
    /// Enable performance metrics
    pub enable_metrics: bool,
}

impl Default for CollectorConfig {
    fn default() -> Self {
        // Use LOW_LATENCY_CONFIG equivalent
        Self {
            ring_bits: 12,       // 4K spans per ring
            max_producers: 16,   // Typical async runtime has 4-8 threads
            enable_metrics: true,
        }
    }
}

impl From<CollectorConfig> for Config {
    fn from(config: CollectorConfig) -> Self {
        Config {
            ring_bits: config.ring_bits,
            max_producers: config.max_producers as usize,
            enable_metrics: config.enable_metrics,
        }
    }
}

/// Metrics for the span collector
#[derive(Debug, Default)]
pub struct CollectorMetrics {
    /// Total spans submitted
    pub spans_submitted: AtomicU64,
    /// Total spans consumed
    pub spans_consumed: AtomicU64,
    /// Number of times ring was full
    pub full_events: AtomicU64,
    /// Number of reserve retries
    pub reserve_retries: AtomicU64,
}

// All methods use `Ordering::Relaxed` because these are purely statistical counters:
//
// 1. No control flow dependencies - No code path depends on these values being "up to date"
// 2. Eventual visibility is acceptable - Slightly stale reads are fine for observability
// 3. No happens-before relationships needed - Unlike ring buffer head/tail, these don't
//    guard any other data or coordinate producer-consumer handoff
// 4. Maximum performance - Relaxed avoids memory barriers in hot paths (submit loops)
//
// This is the standard pattern for metrics/counters in high-performance code.
impl CollectorMetrics {
    #[must_use] 
    pub fn new() -> Self {
        Self::default()
    }

    pub fn spans_submitted(&self) -> u64 {
        self.spans_submitted.load(Ordering::Relaxed)
    }

    pub fn spans_consumed(&self) -> u64 {
        self.spans_consumed.load(Ordering::Relaxed)
    }

    pub fn full_events(&self) -> u64 {
        self.full_events.load(Ordering::Relaxed)
    }

    pub fn reserve_retries(&self) -> u64 {
        self.reserve_retries.load(Ordering::Relaxed)
    }
}

/// Error types for span submission
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SubmitError {
    /// Ring buffer is full
    #[error("ring buffer is full")]
    Full,
    /// Channel is closed
    #[error("channel is closed")]
    Closed,
}

/// Core synchronous span collector using lock-free MPSC channels
pub struct SpanCollector {
    channel: Arc<Channel<Span>>,
    /// One send gate per producer, for a lossless close (ringmpsc INV-CH-05).
    gates: Arc<SendGates>,
    config: CollectorConfig,
    metrics: Arc<CollectorMetrics>,
}

impl SpanCollector {
    /// Creates a new span collector with the given configuration
    #[must_use] 
    pub fn new(config: CollectorConfig) -> Self {
        let channel = Channel::new(config.clone().into());
        Self {
            channel: Arc::new(channel),
            gates: Arc::new(SendGates::new(usize::from(config.max_producers))),
            config,
            metrics: Arc::new(CollectorMetrics::new()),
        }
    }

    /// Registers a new span producer
    pub fn register(&self) -> Result<SpanProducer, ChannelError> {
        let producer = self.channel.register()?;
        Ok(SpanProducer {
            producer: ExclusiveProducer::new(producer),
            gates: Arc::clone(&self.gates),
            metrics: Arc::clone(&self.metrics),
        })
    }

    /// Returns the underlying channel
    #[must_use] 
    pub fn channel(&self) -> &Arc<Channel<Span>> {
        &self.channel
    }

    /// Returns the collector configuration
    #[must_use] 
    pub fn config(&self) -> &CollectorConfig {
        &self.config
    }

    /// Returns collector metrics
    #[must_use] 
    pub fn metrics(&self) -> &Arc<CollectorMetrics> {
        &self.metrics
    }

    /// Claims the collector's consumer handle.
    ///
    /// All consumption goes through the returned [`SpanConsumer`], and only
    /// one can exist at a time, mirroring [`Channel::consumer`].
    pub fn consumer(&self) -> Result<SpanConsumer, ChannelError> {
        Ok(SpanConsumer {
            consumer: self.channel.consumer()?,
            metrics: Arc::clone(&self.metrics),
        })
    }

    /// Closes the collector: new `register()` calls fail, and every existing
    /// producer's ring refuses new spans, so their submits return
    /// [`SubmitError::Closed`]. Spans submitted before the close can still be
    /// consumed: drain for the last time once [`is_quiescent`](Self::is_quiescent)
    /// returns `true`, and every submit that returned `Ok` is in the drain.
    pub fn close(&self) {
        // Gates first: a submit that starts after this fails, and one already
        // in its section finishes before `is_quiescent()` turns true.
        self.gates.close();
        self.channel.close();
    }

    /// `true` once the collector is closed and no submit is still between
    /// its closed check and its commit. A drain after this sees every span
    /// whose submit returned `Ok` (ringmpsc INV-CH-05).
    #[must_use]
    pub fn is_quiescent(&self) -> bool {
        self.gates.is_quiet()
    }
}

/// Handle for consuming spans from the collector.
///
/// Obtained from [`SpanCollector::consumer`]. Wraps the channel's single
/// [`Consumer`], so every method takes `&mut self`.
pub struct SpanConsumer {
    consumer: Consumer<Span>,
    metrics: Arc<CollectorMetrics>,
}

impl SpanConsumer {
    /// Consumes up to `limit` spans from all producers, transferring ownership.
    ///
    /// # Design Note
    ///
    /// This collector only provides owned consumption APIs because `Span` contains
    /// heap-allocated fields (`String`, `Box<HashMap>`) making cloning expensive.
    /// Reference-based consumption would force callers to clone, which defeats the
    /// purpose of zero-copy ring buffer design.
    ///
    /// For generic use cases where `T` might be `Copy`, see the core library's
    /// `Consumer::consume_all_up_to` which provides both reference and owned variants.
    pub fn consume_all_up_to<F>(&mut self, limit: usize, mut f: F) -> usize
    where
        F: FnMut(Span),
    {
        let mut consumed = 0;
        let result = self.consumer.consume_all_up_to_owned(limit, |span| {
            f(span);
            consumed += 1;
        });
        
        if consumed > 0 {
            self.metrics
                .spans_consumed
                .fetch_add(consumed as u64, Ordering::Relaxed);
        }
        
        result
    }

    /// Consumes all available spans from all producers, transferring ownership.
    ///
    /// # Design Note
    ///
    /// This collector only provides owned consumption APIs because `Span` contains
    /// heap-allocated fields (`String`, `Box<HashMap>`) making cloning expensive.
    /// See [`consume_all_up_to`] for detailed rationale.
    pub fn consume_all<F>(&mut self, mut f: F) -> usize
    where
        F: FnMut(Span),
    {
        let mut consumed = 0;
        let result = self.consumer.consume_all_owned(|span| {
            f(span);
            consumed += 1;
        });
        
        if consumed > 0 {
            self.metrics
                .spans_consumed
                .fetch_add(consumed as u64, Ordering::Relaxed);
        }
        
        result
    }
}

/// Handle for submitting spans to the collector
pub struct SpanProducer {
    producer: ExclusiveProducer<Span>,
    gates: Arc<SendGates>,
    metrics: Arc<CollectorMetrics>,
}

/// Holds the `Producer` in its own module, so the tuple field is private to
/// it: nothing else in this file can reach `Producer::push` / `send` through
/// `&self`, and the `unsafe impl Sync` below stays true by construction.
mod exclusive {
    use ringmpsc_rs::{Producer, Reservation};

    /// A `Producer` reached only through `&mut self` for writes.
    ///
    /// `Producer` is `!Sync` because its `push` and `send` take `&self`. This
    /// wrapper doesn't expose them: it forwards `reserve` through `&mut self`,
    /// and through `&self` only
    /// `is_closed`, an atomic load, and `id`, a plain read. So it is safe to share, and `SpanProducer`
    /// gets its `Sync` from the compiler, which still checks every other field.
    pub(super) struct ExclusiveProducer<T>(Producer<T>);

    // Safety: a shared `&ExclusiveProducer` can only call `is_closed`, an atomic
    // load, and `id`, a read of a plain id. Every write to the ring goes through `&mut self`, so it stays on one thread at a time and
    // the ring keeps a single producer (ringmpsc INV-SW-01). Keep it that way:
    // never add a `&self` method that reserves, pushes or sends, and never hand
    // out `&Producer`. `T: Send` matches `Producer`'s own `Send` bound.
    unsafe impl<T: Send> Sync for ExclusiveProducer<T> {}

    impl<T> ExclusiveProducer<T> {
        #[inline]
        pub(super) fn new(producer: Producer<T>) -> Self {
            Self(producer)
        }

        #[inline]
        pub(super) fn reserve(&mut self, n: usize) -> Option<Reservation<'_, T>> {
            self.0.reserve(n)
        }

        #[inline]
        pub(super) fn is_closed(&self) -> bool {
            self.0.is_closed()
        }

        /// The producer's id, which picks its send gate.
        #[inline]
        pub(super) fn id(&self) -> usize {
            self.0.id()
        }
    }
}
use exclusive::ExclusiveProducer;

/// Why a `reserve` returned `None`: a closed ring always refuses, and that
/// isn't backpressure, so it doesn't count as a full event.
fn reserve_failed(producer: &ExclusiveProducer<Span>, metrics: &CollectorMetrics) -> SubmitError {
    if producer.is_closed() {
        SubmitError::Closed
    } else {
        metrics.full_events.fetch_add(1, Ordering::Relaxed);
        SubmitError::Full
    }
}

/// One reserve and commit, with no waiting: the body of a send section.
/// Hands the span back if the ring had no room (or is closed). A free
/// function over the producer, so the section's borrow of the gates can stay
/// alive while it runs.
fn commit_once(producer: &mut ExclusiveProducer<Span>, span: Span) -> Result<(), Span> {
    let Some(mut reservation) = producer.reserve(1) else {
        return Err(span);
    };
    if reservation.push(span).is_err() {
        unreachable!("a one-slot reservation has room for one item");
    }
    reservation.commit();
    Ok(())
}

impl SpanProducer {
    /// Submits a span, retrying with backoff while the ring is full.
    ///
    /// Each attempt is its own send section (one reserve and commit, never a
    /// wait for space, ringmpsc INV-CH-05); the backoff runs between
    /// sections, so it never holds up a closing consumer's final drain.
    ///
    /// # Errors
    ///
    /// [`SubmitError::Closed`] once the collector is closed;
    /// [`SubmitError::Full`] if the ring stays full through the backoff.
    pub fn submit_span(&mut self, span: Span) -> Result<(), SubmitError> {
        let mut backoff = Backoff::new();
        let mut span = span;
        loop {
            let Some(section) = self.gates.begin(self.producer.id()) else {
                return Err(SubmitError::Closed);
            };
            let attempt = commit_once(&mut self.producer, span);
            let _ = section.end();
            match attempt {
                Ok(()) => {
                    self.metrics.spans_submitted.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                Err(back) => span = back,
            }
            if self.producer.is_closed() || backoff.is_completed() {
                return Err(reserve_failed(&self.producer, &self.metrics));
            }
            backoff.snooze();
        }
    }

    /// Tries to submit a span without blocking.
    ///
    /// # Errors
    ///
    /// [`SubmitError::Closed`] once the collector is closed;
    /// [`SubmitError::Full`] if the ring is full.
    pub fn try_submit_span(&mut self, span: Span) -> Result<(), SubmitError> {
        let Some(section) = self.gates.begin(self.producer.id()) else {
            return Err(SubmitError::Closed);
        };
        let attempt = commit_once(&mut self.producer, span);
        let _ = section.end();
        match attempt {
            Ok(()) => {
                self.metrics.spans_submitted.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(_) => Err(reserve_failed(&self.producer, &self.metrics)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::SpanKind;

    /// INV-CH-05, deterministically: a submit that reserved before the close
    /// and commits after it is still drained, because `is_quiescent()` stays
    /// false until its section ends.
    #[test]
    fn close_waits_for_an_open_submit() {
        let collector = SpanCollector::new(CollectorConfig::default());
        let mut producer = collector.register().expect("one producer");

        // Mid-submit: section open, slot reserved and written.
        let section = producer
            .gates
            .begin(producer.producer.id())
            .expect("collector is open");
        let mut reservation = producer.producer.reserve(1).expect("empty ring has room");
        assert!(reservation
            .push(Span::new(1, 7, 0, "in-flight".to_string(), SpanKind::Internal))
            .is_ok());

        collector.close();
        assert!(!collector.is_quiescent(), "a submit is still committing");

        reservation.commit();
        let _ = section.end();
        assert!(collector.is_quiescent());

        let mut ids = Vec::new();
        collector
            .consumer()
            .expect("one consumer")
            .consume_all(|span| ids.push(span.span_id));
        assert_eq!(ids, vec![7], "the in-flight span is drained");
    }

    /// #45 follow-up: on a closed collector, submits report `Closed`, not
    /// `Full`, and don't count as full events.
    #[test]
    fn submit_after_close_is_closed_not_full() {
        let collector = SpanCollector::new(CollectorConfig::default());
        let mut producer = collector.register().expect("one producer");
        collector.close();

        let span = || Span::new(1, 1, 0, "op".to_string(), SpanKind::Internal);
        assert_eq!(producer.submit_span(span()), Err(SubmitError::Closed));
        assert_eq!(producer.try_submit_span(span()), Err(SubmitError::Closed));
        assert_eq!(collector.metrics().full_events(), 0);
    }

    #[test]
    fn span_producer_is_send_and_sync() {
        fn assert_send_sync<S: Send + Sync>() {}
        assert_send_sync::<SpanProducer>();
    }

    #[test]
    fn test_submit_and_consume_span() {
        let collector = SpanCollector::new(CollectorConfig::default());
        let mut producer = collector.register().unwrap();

        let span = Span::new(12345, 1, 0, "test-op".to_string(), SpanKind::Internal);
        producer.submit_span(span).unwrap();

        let mut consumed = Vec::new();
        collector.consumer().unwrap().consume_all(|span| {
            consumed.push(span);  // Zero-copy: ownership transferred
        });

        assert_eq!(consumed.len(), 1);
        assert_eq!(consumed[0].trace_id, 12345);
        assert_eq!(consumed[0].name, "test-op");
    }

    #[test]
    fn test_multiple_producers() {
        let collector = SpanCollector::new(CollectorConfig::default());
        
        let mut producer1 = collector.register().unwrap();
        let mut producer2 = collector.register().unwrap();

        let span1 = Span::new(1, 1, 0, "op1".to_string(), SpanKind::Server);
        let span2 = Span::new(2, 2, 0, "op2".to_string(), SpanKind::Client);

        producer1.submit_span(span1).unwrap();
        producer2.submit_span(span2).unwrap();

        let mut consumed = Vec::new();
        collector.consumer().unwrap().consume_all(|span| {
            consumed.push(span);  // Zero-copy: ownership transferred
        });

        assert_eq!(consumed.len(), 2);
    }

    #[test]
    fn test_metrics() {
        let collector = SpanCollector::new(CollectorConfig::default());
        let mut producer = collector.register().unwrap();

        for i in 0..10 {
            let span = Span::new(u128::from(i), i, 0, format!("op-{i}"), SpanKind::Internal);
            producer.submit_span(span).unwrap();
        }

        assert_eq!(collector.metrics().spans_submitted(), 10);

        collector.consumer().unwrap().consume_all(|_span| {
            // Span is dropped here after processing
        });

        assert_eq!(collector.metrics().spans_consumed(), 10);
    }
}
