//! On-demand WebAssembly bindings for `reticula-table`: what the worker behind
//! a table runs.
//!
//! Files stay with JavaScript ([`file`]). Nothing here takes long: whatever may
//! is divided into steps, and whoever calls decides when to make the next.

mod check;
pub mod csv;
pub mod file;
pub mod json;
pub mod outcome;
mod pages;
mod parquet;
mod query;
mod remote;
pub mod table;
pub mod wire;
pub mod work;

use std::cell::RefCell;

use wasm_bindgen::prelude::*;

thread_local! {
    static PANIC: RefCell<Option<String>> = const { RefCell::new(None) };
}

#[wasm_bindgen(start)]
fn start() {
    std::panic::set_hook(Box::new(|info| {
        let message = info.to_string();
        PANIC.with(|slot| *slot.borrow_mut() = Some(message));
    }));
}

/// What a panic said. A trap without one is for want of memory.
#[wasm_bindgen(js_name = panicMessage)]
#[must_use]
pub fn panic_message() -> Option<String> {
    PANIC.with(|slot| slot.borrow().clone())
}

/// Fails as a defect would, for tests of what happens then.
#[wasm_bindgen]
pub fn crash() {
    panic!("a crash was asked for");
}
