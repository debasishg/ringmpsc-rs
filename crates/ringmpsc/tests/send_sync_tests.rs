//! Compile-time checks that producers are `Send` but not `Sync` (#43).
//!
//! `push` and `send` take `&self`, so a `Sync` producer would let two threads
//! write one ring (INV-SW-01). The `compile_fail` doctests on `Producer` and
//! `StackProducer` show the error, but stable rustdoc doesn't check a
//! doctest's error code, so they would also pass on an unrelated compile
//! error. These checks fail to build only if a producer becomes `Sync`.

use ringmpsc_rs::Producer;
#[cfg(feature = "stack-ring")]
use ringmpsc_rs::StackProducer;

/// `<T as AmbiguousIfSync<_>>::some_item` resolves only if exactly one impl
/// applies. The blanket impl always does. The second applies only when
/// `T: Sync`, which makes the call ambiguous and the build fails (E0283).
trait AmbiguousIfSync<A> {
    fn some_item() {}
}

impl<T: ?Sized> AmbiguousIfSync<()> for T {}

struct IsSync;
impl<T: ?Sized + Sync> AmbiguousIfSync<IsSync> for T {}

macro_rules! assert_not_sync {
    ($t:ty) => {
        let _ = <$t as AmbiguousIfSync<_>>::some_item;
    };
}

fn assert_send<T: Send>() {}

#[test]
fn producer_is_send_not_sync() {
    assert_send::<Producer<u64>>();
    assert_not_sync!(Producer<u64>);
    // `Producer<T>` is `!Sync` for every `T`, including `Sync` ones.
    assert_not_sync!(Producer<String>);
}

#[cfg(feature = "stack-ring")]
#[test]
fn stack_producer_is_send_not_sync() {
    assert_send::<StackProducer<'static, u64, 8, 1>>();
    assert_not_sync!(StackProducer<'static, u64, 8, 1>);
    assert_not_sync!(StackProducer<'static, String, 8, 1>);
}
