//! Files, which JavaScript holds.

use reticula_table::source::{Sink, SinkFailed, Source, SourceError};
use wasm_bindgen::prelude::*;

#[wasm_bindgen(typescript_custom_section)]
const TABLE_FILE: &str = r#"
/** A file as whoever runs this module holds it. None of its functions may throw. */
export interface TableFile {
  /** Fills `into` with the bytes from `at` on. 0: it did; 1: they are not here yet; 2: they cannot be read. */
  readAt(at: number, into: Uint8Array): number;
  /** Adds bytes at the end. Whether it did. */
  append(bytes: Uint8Array): boolean;
}
"#;

#[wasm_bindgen]
extern "C" {
    /// A file as JavaScript holds it.
    #[wasm_bindgen(typescript_type = "TableFile")]
    pub type TableFile;

    #[wasm_bindgen(method, js_name = readAt)]
    fn read_at(this: &TableFile, at: f64, into: &mut [u8]) -> u32;

    #[wasm_bindgen(method)]
    fn append(this: &TableFile, bytes: &[u8]) -> bool;
}

/// Offsets stay below two to the power of 53.
#[expect(clippy::cast_precision_loss, reason = "a file is far shorter")]
pub const fn offset(value: u64) -> f64 {
    value as f64
}

/// What an offset from JavaScript is. Anything that is not one is zero.
pub fn from_offset(value: f64) -> u64 {
    if value.is_finite() && value >= 0.0 {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "checked above"
        )]
        let whole = value as u64;
        whole
    } else {
        0
    }
}

/// A file to read from or write to.
pub struct JsFile(pub TableFile);

impl Source for JsFile {
    fn read(&mut self, at: u64, into: &mut [u8]) -> Result<(), SourceError> {
        match self.0.read_at(offset(at), into) {
            0 => Ok(()),
            1 => Err(SourceError::Missing),
            _ => Err(SourceError::Failed),
        }
    }
}

impl Sink for JsFile {
    fn append(&mut self, bytes: &[u8]) -> Result<(), SinkFailed> {
        if self.0.append(bytes) {
            Ok(())
        } else {
            Err(SinkFailed)
        }
    }
}
