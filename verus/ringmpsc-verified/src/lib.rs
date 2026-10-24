//! Verus proofs for the ringmpsc ring buffer. See README.md.
//!
//! - `arith`: the ring's index and space arithmetic
//! - `protocol`: the SPSC protocol (cursors only), for every capacity
//! - `slots`: the protocol with slot ownership (`PointsTo` tokens)
//! - `ring`: a verified single-threaded exec ring built on `slots`

pub mod arith;
pub mod protocol;
pub mod ring;
pub mod slots;
