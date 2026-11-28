//! Regression tests for #45: closing a channel, and registering while it
//! closes. `loom_tests.rs` checks the registration word itself under every
//! interleaving; these check the behavior users see.

use ringmpsc_rs::{Channel, ChannelError, Config};
use std::sync::Barrier;
use std::thread;

/// Bug 1: a heap producer must not enqueue after `Channel::close()`.
#[test]
fn heap_push_fails_after_channel_close() {
    let channel = Channel::<u64>::new(Config::new(3, 2, false));
    let mut producer = channel.register().expect("one producer");
    assert!(producer.push(1), "open channel accepts a push");

    channel.close();

    assert!(!producer.push(2), "push after close must return false");
    assert_eq!(producer.send(&[3, 4]), 0, "send after close sends nothing");
    assert!(producer.reserve(1).is_none(), "reserve after close is None");
    assert!(producer.reserve_with_backoff(1).is_none());

    // Items pushed before the close can still be drained.
    let mut seen = Vec::new();
    channel
        .consumer()
        .expect("one consumer")
        .consume_all(|v| seen.push(*v));
    assert_eq!(seen, vec![1]);
}

/// Bug 1: the same for `Producer::close()`, which closes only its ring.
#[test]
fn heap_push_fails_after_producer_close() {
    let channel = Channel::<u64>::new(Config::new(3, 2, false));
    let a = channel.register().expect("producer a");
    let b = channel.register().expect("producer b");

    a.close();

    assert!(!a.push(1), "a's ring is closed");
    assert!(b.push(2), "b's ring is still open");
}

/// Bugs 2 and 3: producers register from many threads while another thread
/// closes the channel. Each `register()` either fails with `Closed` or gets a
/// producer whose ring the close reached, so its push fails. Nobody panics,
/// and nobody ends up with an open ring on a closed channel.
#[test]
fn register_racing_close_never_leaves_an_open_ring() {
    const THREADS: usize = 8;
    const ROUNDS: usize = 200;

    for _ in 0..ROUNDS {
        // Fewer slots than threads, so some registrations hit the limit
        // while others race the close.
        let channel = Channel::<u64>::new(Config::new(2, THREADS / 2, false));
        let start = Barrier::new(THREADS + 1);

        thread::scope(|s| {
            let registrars: Vec<_> = (0..THREADS)
                .map(|_| {
                    s.spawn(|| {
                        start.wait();
                        channel.register()
                    })
                })
                .collect();

            start.wait();
            channel.close();
            // Readers that slice `rings[..count]` must not panic while
            // registrations are still in flight.
            let _ = channel.metrics();

            for handle in registrars {
                match handle.join().expect("registrar thread") {
                    Ok(producer) => assert!(
                        !producer.push(7),
                        "producer {} registered but its ring wasn't closed",
                        producer.id()
                    ),
                    Err(ChannelError::Closed | ChannelError::TooManyProducers { .. }) => {}
                    Err(e) => panic!("unexpected error: {e:?}"),
                }
            }
        });

        assert!(channel.producer_count() <= THREADS / 2);
    }
}

/// The stack channel behaves the same.
#[cfg(feature = "stack-ring")]
#[test]
fn stack_register_racing_close_never_leaves_an_open_ring() {
    use ringmpsc_rs::{StackChannel, StackChannelError};

    const THREADS: usize = 8;
    const ROUNDS: usize = 200;

    for _ in 0..ROUNDS {
        let channel: StackChannel<u64, 4, 4> = StackChannel::new();
        let start = Barrier::new(THREADS + 1);

        thread::scope(|s| {
            let registrars: Vec<_> = (0..THREADS)
                .map(|_| {
                    s.spawn(|| {
                        start.wait();
                        channel.register()
                    })
                })
                .collect();

            start.wait();
            channel.close();

            for handle in registrars {
                match handle.join().expect("registrar thread") {
                    Ok(producer) => assert!(
                        !producer.push(7),
                        "producer {} registered but its ring wasn't closed",
                        producer.id()
                    ),
                    Err(StackChannelError::Closed | StackChannelError::TooManyProducers { .. }) => {}
                    Err(e) => panic!("unexpected error: {e:?}"),
                }
            }
        });

        assert!(channel.producer_count() <= 4);
    }
}
