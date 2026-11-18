# Reticula

A local-first spreadsheet for the browser backed by an identity-addressed `#![no_std]` Rust calculation kernel compiled to WebAssembly.

## Architecture

Reticula splits rendering and calculation cleanly across threads:

- **Kernel (`crates/core`, `crates/formula`)** — `#![no_std]` + `alloc` Rust crates verified against `wasm32v1-none`:
  - **64-bit stable row and column identities** (`8-bit epoch | 24-bit replica | 32-bit counter`) stored on run-encoded axes with tombstone counts, paired with `4,096`-row adaptive `Empty` / `Sparse` / `Dense` columnar blocks and 8-byte NaN-boxed cell values.
  - **Formula engine** — Pratt parser, coordinate-to-ID binder (`BoundFormula`), Excel-compatible 15-significant-digit comparisons/rounding and outermost cancellation snapping (verified against 22,968 recorded Excel 16.0 cases), dynamic-array broadcasting and legacy implicit intersection, 39 Layer-0 built-in functions, anchor-ordered range dependency index, and a non-recursive heap-stack evaluator with `#CYCLE!` detection and 4 ms time-sliced incremental recalculation.
  - **Segmented binary snapshots (`RTCLSNAP` v0)** — `postcard` + bounded `lz4_flex` block compression with strict NaN bit-pattern and length validation on decode.
- **Worker & Viewport Protocol (`crates/wasm`, `packages/protocol`)** — TypeScript message definitions are generated from Rust via `tsify`, and visible cells cross from the Web Worker to the main thread as zero-copy `ArrayBuffer` viewport pages (`64 × 16` cells, 64-bit row/column IDs, UTF-8 display text).
- **Canvas2D Grid (`packages/grid`, `apps/web`)** — Virtualized 2D canvas renderer with custom scrollbars, adaptive prefetch margins, rectangular range selection, synced formula bar, and an always-focused hidden CodeMirror 6 cell editor for CJK input method composition.
- **Static & Sync Host (`crates/server`)** — `axum` + `tower-http` server negotiating build-time precompressed `.br` and `.gz` assets with immutable asset caching, `application/wasm` streaming headers, SPA fallback, and `/healthz`.

## Measured Performance (M1a)

Measured on an Intel Core i9-13980HX (pinned to P-cores `0xFFFF`, 32 GB RAM, Windows 11, Google Chrome `158.0.8127.36`, WebAssembly `-O3`); raw data archived under [`bench/results/`](bench/results/):

| Workload | Chrome WASM | Native (`x86_64`) | Target |
|---|---:|---:|---:|
| **B2** — `=SUM(A1:A1000000)` over 1M rows | `2.04 ms` (`p50`) | `1.65 ms` (`p50`) | — |
| **B3** — `1,000,000` formulas `=B2*C2+$Z$1` full recalc | `456.3 ms` (`p50`) | `548.2 ms` (`p50`) | baseline |
| **B4** — `100,000` formulas `=A2*$Z$1` incremental recalc | **`24.18 ms`** (`p50`) | `24.71 ms` (`p50`) | `≤ 50 ms` |
| **B5** — `1,000,000`-row chain `=C1+B2` (non-recursive) | `609.0 ms` (`p50`) | `623.3 ms` (`p50`) | no stack overflow |
| **B7a** — Insert 1 row at top of 1M-row / 1M-formula sheet | **`1.7 µs`** (mean of 100) | `1.0 µs` (`p50`) | `≤ 20 ms` |
| **B10** — Cold first visit (`20 Mbit/s`, `50 ms` RTT, 2-RTT TLS) | **`370 ms`** shell / **`765 ms`** first screen | — | `≤ 1.5 s` / `≤ 2.0 s` |
| **1M × 22 Scroll** — 5 screens/s continuous scroll frame work | **`0.66 ms`** `p50` / **`0.93 ms`** `p99` | — | `p99 ≤ 16.7 ms` |
| **Bundle Size** — Core `.wasm` / first-screen JS (brotli-11) | **`195.9 KB`** / **`129.1 KB`** | — | `≤ 1.2 MB` WASM |

## Development

Requires [rustup](https://rustup.rs) (installs the pinned Rust `1.100.0` toolchain automatically), Node.js 26, and pnpm. All tasks go through `cargo xtask`:

```sh
cargo xtask setup --browsers        # install pinned tools into .tools/, JS deps, and Playwright browsers
cargo xtask dev                     # start local Vite dev server on http://localhost:5273
cargo xtask check                   # formatting, clippy (native + wasm32 + wasm32v1-none), codegen, biome, tsc
cargo xtask test                    # native nextest, wasm32 kernel tests, property tests, and Vitest
cargo xtask diff --count 2500       # differential formula testing against ironcalc_base & formualizer-eval
cargo xtask bench --affinity FFFF   # run and archive native + Chrome WASM engine benchmarks
cargo xtask ci                      # full pre-push verification (check, test, deny, release build, 3-browser E2E)
```

Run `cargo xtask` with no arguments to see all available commands.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

## Contributing

Bug reports and questions are welcome through issues. Pull requests are not being accepted at this stage.
