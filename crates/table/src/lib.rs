//! Read-only tables of many millions of rows.
//!
//! A table is one immutable file of columns, described in [`layout`]. This
//! crate writes it ([`writer`]), reads single cells and whole blocks of it
//! ([`reader`]), shows its cells ([`text`], [`page`]), and answers questions
//! about it ([`query`]). Tables come from other files through [`csv`] and
//! [`parquet`]; [`sample`] makes one up.
//!
//! The crate belongs to the engine kernel. Kernel crates perform no I/O, use
//! no platform APIs, never panic, and produce bit-identical results on every
//! target. Bytes come from a [`source::Source`] and go to a
//! [`source::Sink`]; a source may answer that it does not have them yet,
//! which is how a file that is still being fetched is read.

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

pub mod bits;
pub mod calendar;
pub mod csv;
pub mod determinism;
pub mod hash;
pub mod layout;
pub mod page;
pub mod parquet;
pub mod query;
pub mod reader;
pub mod sample;
pub mod source;
pub mod text;
pub mod types;
pub mod writer;

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests of small tables"
)]
mod tests;
