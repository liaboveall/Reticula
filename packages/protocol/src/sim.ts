// The deterministic sync simulator WebAssembly bindings, loaded on demand by
// `/sim` so the spreadsheet app never downloads or parses it.
export * from "../sim-pkg/reticula_sim_wasm.js";
export { default as init } from "../sim-pkg/reticula_sim_wasm.js";
