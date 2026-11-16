//! Data model and numeric foundations of the Reticula engine.
//!
//! This crate belongs to the engine kernel. Kernel crates perform no I/O, use
//! no platform APIs, never panic, and produce bit-identical results on every
//! target. They are `no_std` so that the first two properties are enforced by
//! the compiler rather than by review: files, sockets, clocks and threads do
//! not exist here, and neither do the platform-dependent `f64` methods.

#![no_std]
// --- kernel lint block: identical in every kernel crate, verified by `cargo xtask check` ---
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::exit,
    clippy::expect_used,
    clippy::get_unwrap,
    clippy::indexing_slicing,
    clippy::iter_over_hash_type,
    clippy::panic,
    clippy::string_slice,
    clippy::todo,
    clippy::unimplemented,
    clippy::unreachable,
    clippy::unwrap_used
)]
// --- end kernel lint block ---

#[cfg(test)]
extern crate std;

pub mod determinism;
pub mod digest;
pub mod num;
pub mod rng;

/// Version of the kernel, recorded alongside cached results so that they can
/// be discarded when the code that produced them changes.
pub const KERNEL_VERSION: &str = env!("CARGO_PKG_VERSION");
