// The bindings of the engine behind read-only tables, and the types of what it
// is told and tells. Only the worker that holds a table loads them.
export * from "../table-pkg/reticula_table_wasm.js";
export { default as init } from "../table-pkg/reticula_table_wasm.js";
