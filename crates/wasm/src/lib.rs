//! WebAssembly bindings: the entry point of the engine worker.
//!
//! The engine is driven like a state machine. A command goes in through
//! [`Engine::handle`], which may answer with an event; viewport pages the
//! command made due are then taken out one at a time with
//! [`Engine::take_page`]. The engine never posts a message itself, so the glue
//! around it decides how bytes travel.
//!
//! Everything that crosses the worker boundary as a structured object is
//! defined here and exported to TypeScript, so there is exactly one definition
//! of each type.

use std::collections::VecDeque;

use reticula_core::sample;
use reticula_core::sheet::Sheet;
use reticula_core::viewport::{self, Page, PageRange};
use reticula_formula::determinism;
use serde::{Deserialize, Serialize};
use tsify::{Ts, Tsify};
use wasm_bindgen::prelude::*;

/// A message from the main thread to the engine worker.
#[derive(Debug, Deserialize, Tsify)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum EngineCommand {
    /// Replace the document with the seeded sample. Answered by
    /// [`EngineEvent::Ready`].
    OpenSample {
        /// Seed of the sample. The same seed gives the same sheet everywhere.
        seed: u32,
        /// Number of rows, the header row included.
        rows: u32,
    },
    /// Declare the range of cells the grid wants to hold: what it shows plus a
    /// margin around it. This replaces the previous subscription. The engine
    /// sends the pages that cover the range and that the previous subscription
    /// did not; the grid drops the pages that fell out of it.
    Subscribe {
        /// A number the grid picks; every page sent for this subscription
        /// carries it.
        epoch: u32,
        /// Position of the first row of the range.
        first_row: u32,
        /// Number of rows in the range.
        row_count: u32,
        /// Position of the first column of the range.
        first_col: u32,
        /// Number of columns in the range.
        col_count: u32,
    },
    /// Run the determinism self-check. Answered by [`EngineEvent::SelfChecked`].
    SelfCheck,
}

/// A message from the engine worker to the main thread.
#[derive(Debug, Serialize, Tsify)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum EngineEvent {
    /// A document is open and its cells can be subscribed to.
    Ready {
        /// Facts about the engine.
        info: EngineInfo,
        /// The sheet that was opened.
        sheet: SheetInfo,
    },
    /// The determinism self-check has run.
    SelfChecked {
        /// Its result: 16 lowercase hexadecimal digits. It is the same in
        /// every browser and on every native target.
        digest: String,
    },
}

/// Facts about the running engine.
#[derive(Debug, Serialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct EngineInfo {
    /// Version of the engine kernel.
    pub kernel_version: String,
}

/// Facts about an open sheet.
#[derive(Debug, Serialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct SheetInfo {
    /// Number of rows.
    pub rows: u32,
    /// Number of columns.
    pub cols: u32,
    /// Seed the sheet was generated from.
    pub seed: u32,
    /// How long generating it took, in milliseconds.
    pub generation_ms: f64,
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(message: &str);

    /// The worker's monotonic clock, in milliseconds.
    #[wasm_bindgen(js_namespace = performance, js_name = now)]
    fn performance_now() -> f64;
}

/// Runs once when the module is instantiated.
#[wasm_bindgen(start)]
fn start() {
    // A panic in the engine is a bug. Report it where a developer will see it;
    // the worker glue turns the resulting trap into a prompt to reload.
    std::panic::set_hook(Box::new(|info| {
        console_error(&format!("engine panic: {info}"));
    }));
}

/// The engine: one open document and the grid's subscription to it.
#[wasm_bindgen]
#[derive(Debug, Default)]
pub struct Engine {
    sheet: Sheet,
    /// The pages the grid holds or is about to be sent.
    subscribed: PageRange,
    /// The subscription that pages are currently sent for.
    epoch: u32,
    /// Pages that are due, in the order they will be sent.
    due: VecDeque<Page>,
    /// Scratch space for encoding pages.
    text: String,
}

#[wasm_bindgen]
impl Engine {
    /// Creates an engine with an empty document.
    #[wasm_bindgen(constructor)]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Carries out a command and returns the event that answers it, if the
    /// command has one. Afterwards, take pages until there are none left.
    ///
    /// # Errors
    ///
    /// Fails if `command` is not a well-formed command; the engine is left
    /// unchanged.
    pub fn handle(
        &mut self,
        command: &Ts<EngineCommand>,
    ) -> Result<Option<Ts<EngineEvent>>, JsError> {
        let event = match command.to_rust()? {
            EngineCommand::OpenSample { seed, rows } => Some(self.open_sample(seed, rows)),
            EngineCommand::Subscribe {
                epoch,
                first_row,
                row_count,
                first_col,
                col_count,
            } => {
                self.subscribe(epoch, (first_row, row_count), (first_col, col_count));
                None
            }
            EngineCommand::SelfCheck => {
                let digest = determinism::to_hex(determinism::digest());
                Some(EngineEvent::SelfChecked {
                    digest: String::from_utf8_lossy(&digest).into_owned(),
                })
            }
        };
        Ok(event.as_ref().map(Ts::from_rust).transpose()?)
    }

    /// Encodes the next page that is due and hands it over, or returns
    /// nothing when the grid has been sent everything it subscribed to.
    #[wasm_bindgen(js_name = takePage)]
    pub fn take_page(&mut self) -> Option<Vec<u8>> {
        let page = self.due.pop_front()?;
        Some(viewport::encode_page(
            &self.sheet,
            page,
            self.epoch,
            &mut self.text,
        ))
    }
}

impl Engine {
    fn open_sample(&mut self, seed: u32, rows: u32) -> EngineEvent {
        let started = performance_now();
        self.sheet = sample::generate(u64::from(seed), rows);
        let generation_ms = performance_now() - started;

        // The grid holds nothing of the new document.
        self.subscribed = PageRange::default();
        self.due.clear();
        EngineEvent::Ready {
            info: EngineInfo {
                kernel_version: reticula_core::KERNEL_VERSION.to_owned(),
            },
            sheet: SheetInfo {
                rows: self.sheet.rows(),
                cols: self.sheet.cols(),
                seed,
                generation_ms,
            },
        }
    }

    fn subscribe(&mut self, epoch: u32, rows: (u32, u32), cols: (u32, u32)) {
        let wanted = PageRange::covering((self.sheet.rows(), self.sheet.cols()), rows, cols);
        let held = self.subscribed;
        // A page that is still due was promised under the previous
        // subscription; it is sent only if this one still wants it.
        self.due.retain(|page| wanted.contains(*page));
        self.due
            .extend(wanted.pages().filter(|page| !held.contains(*page)));
        self.subscribed = wanted;
        self.epoch = epoch;
    }
}
