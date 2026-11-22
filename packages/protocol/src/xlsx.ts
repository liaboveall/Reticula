// The on-demand XLSX import/export/patch WebAssembly bindings, loaded lazily
// only when the user imports or exports an `.xlsx` file.
export * from "../xlsx-pkg/reticula_xlsx_wasm.js";
export { default as init } from "../xlsx-pkg/reticula_xlsx_wasm.js";
