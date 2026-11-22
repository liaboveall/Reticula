//! Formula parser, coordinate-to-identity binder, D-027/D-028 evaluator and
//! dependency-driven incremental recalculation.
//!
//! Like `reticula-core`, this crate belongs to the `no_std` engine kernel: it
//! performs no I/O, touches no platform APIs, never panics on any input, and
//! produces bit-identical results across every supported target.

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

pub mod address;
pub mod ast;
pub mod binder;
pub mod dep_graph;
pub mod determinism;
pub mod engine;
pub mod functions;
pub mod lang_service;
pub mod lexer;
pub mod parser;
pub mod spill;
pub mod value_eval;
