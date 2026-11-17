// The engine's JavaScript bindings and every type that crosses the worker
// boundary. All of it is generated from the Rust sources by `cargo xtask wasm`;
// nothing in this package is written by hand, so each type has one definition.
//
// The layout of the binary viewport pages is generated too, by `cargo xtask
// codegen`, and is exported separately as `@reticula/protocol/viewport` so
// that reading a page does not require the engine.
export * from "../pkg/reticula_wasm.js";
export { default as init } from "../pkg/reticula_wasm.js";
