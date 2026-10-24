//! Two consumers on one ring, reachable from safe code.
//!
//! `Channel` is `Clone + Sync` and `consume_all_owned` takes `&self`, so two
//! threads can drain the same ring at once. Each loads `head = 0, tail = 1`,
//! moves slot 0 out with `assume_init_read`, and the one `Box` ends up owned
//! twice: a double free.
//!
//! The `Barrier` inside the handler makes this deterministic. A handler runs
//! after its thread has read the slot and before it stores the new head, so
//! while the first thread waits there, the second must still see the old
//! head and read the same slot. If only one thread could reach the handler,
//! the barrier would never release and the test would deadlock instead.
//!
//! This is UB, so natively it aborts the test binary. It only runs under Miri:
//!
//! ```text
//! cargo +nightly miri test --test miri_double_consumer_test
//! ```

use ringmpsc_rs::{Channel, Config};
use std::sync::Barrier;
use std::thread;

#[test]
#[cfg_attr(not(miri), ignore = "double free: only meaningful under Miri")]
fn two_consumers_drain_the_same_slot() {
    let channel = Channel::<Box<u64>>::new(Config::new(2, 1, false));
    let producer = channel.register().unwrap();
    assert!(producer.push(Box::new(42)));

    let other = channel.clone();
    let both_hold_the_item = Barrier::new(2);

    thread::scope(|s| {
        s.spawn(|| {
            channel.consume_all_owned(|item| {
                both_hold_the_item.wait();
                drop(item);
            })
        });
        s.spawn(|| {
            other.consume_all_owned(|item| {
                both_hold_the_item.wait();
                drop(item);
            })
        });
    });
}
