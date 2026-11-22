//! Frozen versioned persistence formats (`v1`), Copy-on-Write streaming
//! snapshots (`RTCLSNAP` v1) and CRC-32 framed write-ahead logs (`RTCLWAL1`).
//!
//! Like `reticula-core` and `reticula-formula`, this crate belongs to the
//! pure `no_std` engine kernel: zero I/O, zero platform APIs, zero `unsafe`,
//! and zero panics on any untrusted or fuzzed byte stream.

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

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod convert_v1;
pub mod convert_v2;
pub mod convert_v3;
pub mod crc32;
pub mod snapshot;
pub mod sync_frame;
pub mod v1;
pub mod v2;
pub mod v3;
pub mod wal;
