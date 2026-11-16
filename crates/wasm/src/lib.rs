//! WebAssembly bindings: the entry point of the engine worker.
//!
//! Everything that crosses the worker boundary as a structured object is
//! defined here and exported to TypeScript, so there is exactly one definition
//! of each type.

use reticula_core::determinism;
use serde::Serialize;
use tsify::{Ts, Tsify};
use wasm_bindgen::prelude::*;

/// Facts about the running engine, reported once after start-up.
#[derive(Debug, Serialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct EngineInfo {
    /// Version of the engine kernel.
    pub kernel_version: String,
    /// Result of the determinism self-check: 16 lowercase hexadecimal digits.
    /// It is the same in every browser and on every native target.
    pub determinism_digest: String,
}

/// A message from the engine worker to the main thread.
#[derive(Debug, Serialize, Tsify)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum EngineEvent {
    /// The engine has started and can accept commands.
    Ready {
        /// Facts about the engine that just started.
        info: EngineInfo,
    },
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(message: &str);
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

/// Starts the engine and returns the event announcing that it is ready.
///
/// # Errors
///
/// Fails only if the event cannot be converted to a JavaScript value.
#[wasm_bindgen]
pub fn boot() -> Result<Ts<EngineEvent>, JsError> {
    let digest = determinism::to_hex(determinism::digest());
    let event = EngineEvent::Ready {
        info: EngineInfo {
            kernel_version: reticula_core::KERNEL_VERSION.to_owned(),
            determinism_digest: String::from_utf8_lossy(&digest).into_owned(),
        },
    };
    Ok(Ts::from_rust(&event)?)
}
