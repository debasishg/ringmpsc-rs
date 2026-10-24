//! Verus proofs for the ringmpsc ring buffer. See README.md.
//!
//! - `arith`: the ring's index and space arithmetic
//! - `protocol`: the SPSC protocol (cursors only), for every capacity
//! - `slots`: the protocol with slot ownership (`PointsTo` tokens)
//! - `ring`: a verified single-threaded exec ring built on `slots`
//! - `spsc`: a verified concurrent ring: a producer half and a consumer half
//! - `channel`: a verified MPSC channel: N `spsc` rings, registration, and the consumer claim

pub mod arith;
pub mod channel;
pub mod protocol;
pub mod ring;
pub mod slots;
pub mod spsc;
