//! Prototype of identity addressing. Not part of the engine.
//!
//! Rows have stable identities; cells and formula references are bound to
//! identities, not to positions. The prototype answers two questions before
//! the real data model is written:
//!
//! - [`model`]: does that give the behaviour a spreadsheet user expects when
//!   rows are inserted, deleted, moved and sorted, alone and concurrently? The
//!   tests compare it with what Excel was recorded to do.
//! - the cost of the extra indirection (added separately).

pub mod model;
pub mod sync;
