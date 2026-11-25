//! What steps come to. Most say so with one number; why something went wrong is
//! asked for afterwards.

use std::cell::RefCell;

use wasm_bindgen::prelude::*;

use crate::json::json;
use crate::wire::TableTrouble;

/// What a step of something came to.
#[wasm_bindgen]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableStep {
    /// There is more to do.
    Working,
    /// It is done.
    Done,
    /// More of the text is needed, or that it has ended.
    NeedInput,
    /// Bytes have to be here first.
    Need,
    /// What was written is to be thrown away, and the file to be read again. Of a
    /// question: it is to be asked again.
    Restart,
    /// It cannot be done: see `lastTrouble`.
    Failed,
}

impl TableStep {
    /// What it comes to when there may be more to do.
    pub const fn of(is_more: bool) -> Self {
        if is_more { Self::Working } else { Self::Done }
    }
}

thread_local! {
    static TROUBLE: RefCell<TableTrouble> = const { RefCell::new(TableTrouble::Io) };
}

/// Why something failed last.
#[must_use]
#[wasm_bindgen(js_name = lastTrouble, unchecked_return_type = "Json<TableTrouble>")]
pub fn last_trouble() -> String {
    TROUBLE.with_borrow(json)
}

/// Something cannot be done.
pub fn failed(trouble: impl Into<TableTrouble>) -> TableStep {
    let trouble = trouble.into();
    TROUBLE.with_borrow_mut(|last| *last = trouble);
    TableStep::Failed
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use reticula_table::reader::ReadError;

    use super::*;

    #[test]
    fn what_went_wrong_last_is_known_until_something_else_does() {
        assert_eq!(failed(ReadError::Corrupt("x")), TableStep::Failed);
        assert_eq!(last_trouble(), r#"{"reason":"corrupt","detail":"x"}"#);
        assert_eq!(
            failed(TableTrouble::UnclosedQuote { row: 3 }),
            TableStep::Failed
        );
        assert_eq!(last_trouble(), r#"{"reason":"unclosedQuote","row":3}"#);
    }
}
