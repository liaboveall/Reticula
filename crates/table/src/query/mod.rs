//! Filters, orders and totals over a table.
//!
//! A [`Session`] has a table open and shows a view of it: the rows that the
//! conditions of a [`Spec`] hold for, in the order it says. A view that shows
//! every row in the order of the file takes no memory; any other one takes
//! four bytes a row. Building a view, adding up columns and listing the values
//! of a column are jobs that are done in small steps ([`Session::step`]), so
//! that whoever drives them can do other things in between, and can stop. The
//! view that is there stays until the next one is complete.
//!
//! Every column of a view has conditions of its own type ([`Condition`]). One
//! that is not for columns of that type is refused. The conditions are those
//! that the filters of a sheet have, with these differences:
//!
//! | condition | a sheet | a table |
//! |---|---|---|
//! | one of some values | compares what cells show | compares the values that are stored, floating-point numbers bit by bit, texts regardless of case |
//! | a text contains, begins, ends | asks numbers for what they show as well | for text columns only |
//! | a number compares | as floating-point numbers | exactly, with a decimal threshold of any length |
//! | dates and times compare | - | for columns of dates and times |
//! | top and bottom | numbers | numbers, dates and times |
//! | not blank | - | for all columns |
//!
//! What is the same: conditions on several columns all have to hold; texts are
//! compared in lower case; the empty text is a blank; a text condition without
//! a text holds for every cell, also for blank ones, and other conditions than
//! a list of values never hold for them; both thresholds of a range belong to
//! it, whichever comes first; top and bottom values are those of the whole
//! column, whatever the other conditions, and all that equal the last of them
//! belong to them.
//!
//! A threshold of floating-point numbers stands for the one nearest to it:
//! 0.1 in such a column equals the threshold `0.1`, although neither of the
//! two numbers is a tenth exactly. Nothing else is rounded.
//!
//! Orders are stable, and blank cells come last in both directions. Texts are
//! ordered like in sheets: in lower case, byte by byte, and texts that differ
//! only in case are equal.

mod arrange;
mod build;
mod cells;
mod context;
mod exact;
mod filter;
mod fold;
mod memory;
mod predicate;
mod radix;
mod ranks;
mod selection;
mod session;
mod sort;
mod space;
mod spec;
mod summary;
mod top;
mod values;

#[cfg(all(test, not(target_arch = "wasm32")))]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    reason = "measurements"
)]
mod bench;
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

pub use session::{Job, Progress, Session};
pub use spec::{Condition, NumberOp, SortKey, Spec, SpecError, TimeOp, Value};
pub use summary::{ColumnSummary, Figure};
pub use values::{ListedValue, ValueList};

use crate::reader::ReadError;

/// Why a job cannot be completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryError {
    /// The table cannot be read.
    Read(ReadError),
    /// There is not enough memory, or not as much as is allowed.
    OutOfMemory,
    /// Too many texts of a column begin alike to be put in order.
    TooManyEqualPrefixes,
}

impl From<ReadError> for QueryError {
    fn from(error: ReadError) -> Self {
        match error {
            ReadError::OutOfMemory => Self::OutOfMemory,
            other => Self::Read(other),
        }
    }
}
