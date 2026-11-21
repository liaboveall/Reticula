# Reticula

A local-first spreadsheet for the browser backed by an identity-addressed `#![no_std]` Rust calculation kernel compiled to WebAssembly, with crash-safe OPFS persistence, offline Service Worker support, and real-time collaboration through a server-ordered sync protocol that keeps editing offline and merges on reconnect.

## Architecture

Reticula splits rendering, calculation, wire persistence, and local storage cleanly across crates and threads:

- **Kernel (`crates/core`, `crates/formula`, `crates/wire`, `crates/sync`)** — `#![no_std]` + `alloc` Rust crates verified against `wasm32v1-none`:
  - **64-bit stable row and column identities** (`8-bit epoch | 24-bit replica | 32-bit counter`) stored on run-encoded axes with tombstone counts, paired with `4,096`-row adaptive `Empty` / `Sparse` / `Dense` Copy-on-Write (`Arc`) columnar blocks and 8-byte NaN-boxed cell values.
  - **Multi-sheet operations, styles, and CSV** — Reversible sheet operations (`AddSheet`, `DeleteSheet`, `RenameSheet`, `ReorderSheet`), row/column moves with anchor handoff, sparse row-height/column-width overrides, freeze panes, layered cell/row/column styles (`StyleTable`), constant-size `FillFormula` templates, and RFC 4180 CSV/TSV import/export.
  - **Formula engine & language service** — Pratt parser with error recovery, coordinate-to-ID binder (`BoundFormula`), Excel-compatible 15-significant-digit comparisons/rounding and outermost cancellation snapping (verified against 22,968 recorded Excel 16.0 cases), dynamic-array broadcasting and legacy implicit intersection, **69 built-in functions** (Layer 0 + Layer 1 lookup, conditional aggregation, text, date/time, math, and volatile functions), anchor-ordered range dependency graph, non-recursive 4 ms time-sliced evaluator with `#CYCLE!` detection, and single-pass formula analysis (syntax tokens, diagnostics, completions, signature help, colored reference spans, `F4` absolute/relative cycling).
  - **Sync state machines (`crates/sync`)** — Pure `ClientSyncMachine` and `ServerDocMachine` with no I/O: the server orders transactions; a client keeps its unconfirmed ones in a pending queue and, when a remote commit arrives, rewinds them, applies the commit and replays them in place. Per-replica high watermarks make retries idempotent and detect lost tails, log epochs keep replica IDs unique across restores, rejected transactions are saved as a separate local workbook, and a versioned 192-bit `StateDigest` (128-bit additive content digest + 64-bit axis-order digest, formula caches excluded) catches divergence. Undo in a shared workbook is a conditional write (`SetCellsIf`) that skips cells someone else has changed since.
  - **Frozen Wire v1 and v2 (`crates/wire`)** — Wire v2 adds `SetCellsIf`, the version-1 sync frames and `RTCLWAL2` pending logs next to an untouched v1; both schemas and their fixtures are SHA-256 frozen under `tests/compat/`. `RTCLSNAP` v1 segmented Copy-on-Write streaming snapshots (with `SeedOrigin` stubs, orphan string compaction, replica watermarks, and backward-compatible v0 decoding) and `RTCLWAL1` CRC32C-framed write-ahead logs distinguishing torn crash tails from mid-file corruption. Frozen v1 schema and fixtures under `tests/compat/v1/` are verified by SHA-256 on every CI run.
- **Worker, Persistence & Viewport Protocol (`crates/wasm`, `packages/protocol`, `apps/web/src/engine`)** — TypeScript message definitions generated from Rust via `tsify`, zero-copy `ArrayBuffer` viewport pages (`VERSION = 2`, `64 × 16` cells with style IDs, flags, and left-overflow text records), OPFS `FileSystemSyncAccessHandle` double-buffered snapshots (`snapshot-a.bin` / `snapshot-b.bin` + `wal.log` + `state.json`), Web Locks single-instance guard with four-step cross-tab takeover (including hung-worker watchdog and Safari `InvalidStateError` recovery), startup crash sentinel + three-option Safe Mode, and a hand-written Service Worker offline shell with multi-tab coordinated updates.
- **Canvas2D Grid & Editor (`packages/grid`, `apps/web`)** — Virtualized 2D canvas renderer with freeze panes, sparse row/column sizing, multi-range selection, fill handle with series recognition, rich TSV/HTML clipboard interop, CodeMirror 6 formula bar powered by the Rust language service, formatting ribbon, sheet tab bar, and an always-focused hidden CodeMirror 6 cell editor for CJK input method composition.
- **Deterministic simulation (`crates/sim`)** — Several clients and a server in one seeded, single-threaded world with every fault the protocol has to survive (reordering, slow and dropped connections, client and server crashes, OPFS tail loss, epoch restores, permission downgrades, rejects, two tabs on one replica) and ten invariants, checked after every step or once the world goes quiet: among them, every client's visible state equals a fresh replay of its pending queue, no acknowledged transaction is lost, and every replica converges on the server's digest. Failing traces are minimized automatically; 5,000 seeds run on every push and 100,000 nightly. A sixth fuzz target, `fuzz_sync`, feeds arbitrary frames to both machines.
- **Collaboration server (`crates/server`)** — `axum` + `sqlx` on PostgreSQL 18: one actor per open workbook, group commit sized from measurements on the target host (5 ms window, 32 transactions or 256 KiB per flush) and acknowledgement only after the transaction is durable, snapshots on a disk volume, GitHub sign-in with PKCE and hashed session tokens, editor share links (optionally single-use), a binary WebSocket sync endpoint with presence, and an OpenAPI description from which the TypeScript client is generated. Integration tests cover the permission matrix over HTTP and WebSocket and a server killed mid-write that must keep every acknowledged transaction. The same binary serves the precompressed static build. `infra/compose.yml` runs it with PostgreSQL, volumes and the target host's resource limits.
- **Collaborative client (`apps/web`)** — Loaded on demand outside the first-screen bundle: each tab claims its own replica directory in OPFS, pending transactions are written to disk before they are sent, a closed tab's unsent edits are taken over when the workbook is opened again, and remote cursors, selections and name tags are resolved by stable row and column IDs, so they follow inserted and deleted rows; so does the local cursor when someone else moves the rows under it.

## Measured Performance (M1b)

Measured on an Intel Core i9-13980HX (pinned to P-cores `0xFFFF`, 32 GB RAM, Windows 11, Google Chrome `158.0.8127.36`, WebAssembly `-O3`, real disk-backed OPFS `FileSystemSyncAccessHandle`); raw reports archived under [`bench/results/`](bench/results/):

| Workload | Chrome WASM | Native (`x86_64`) | Target |
|---|---:|---:|---:|
| **B1** — `1M × 20` (20M cells + 1M formulas) OPFS load → first page | **`747.7 ms`** (`p50`) | `753.0 ms` (`p50`) | `≤ 2,000 ms` |
| **B1 (numeric)** — `1M × 20` (20M numeric cells, 0 formulas) OPFS load | **`142.5 ms`** (`p50`) | `56.8 ms` (`p50`) | control |
| **B11** — `20M + 1M` formulas background CoW OPFS save / 25 ms probe | **`420.0 ms`** / **`4.75 ms`** (`p95`) | `365.8 ms` (`p50`) | `≤ 2,500 ms` / `p95 ≤ 16 ms` |
| **B12** — `1M × 50` (50M cells + 100k formulas) OPFS load | **`407.2 ms`** (`p50`) | `319.6 ms` (`p50`) | `≤ 3,000 ms` |
| **B12** — `50M + 100k` CoW OPFS save / 25 ms probe | **`613.4 ms`** / **`13.57 ms`** (`p95`) | `399.7 ms` (`p50`) | `≤ 4,000 ms` / `p95 ≤ 16 ms` |
| **B12** — `50M + 100k` dependent recalc / peak WASM memory | **`31.72 ms`** / **`605.2 MiB`** | `35.83 ms` (`p50`) | `≤ 50 ms` / `≤ 1.2 GiB` |
| **B2** — `=SUM(A1:A1000000)` over 1M rows | `1.97 ms` (`p50`) | `1.56 ms` (`p50`) | — |
| **B3** — `1,000,000` formulas `=B2*C2+$Z$1` full recalc | `405.1 ms` (`p50`) | `503.2 ms` (`p50`) | baseline |
| **B4** — `100,000` formulas `=A2*$Z$1` incremental recalc | **`23.34 ms`** (`p50`) | `23.79 ms` (`p50`) | `≤ 50 ms` |
| **B5** — `1,000,000`-row chain `=C1+B2` (non-recursive) | `581.1 ms` (`p50`) | `542.3 ms` (`p50`) | no stack overflow |
| **B6** — `100k` exact `=VLOOKUP(..., FALSE)` over `100k` rows | `77.98 s` (`p50`) | `36.71 s` (`p50`) | linear baseline |
| **B7a** — Insert 1 row at top of 1M-row / 1M-formula sheet | **`1.9 µs`** (mean of 100) | `1.0 µs` (`p50`) | `≤ 20 ms` |
| **B8** — Memory: 10M nums / 1M rep str / 1M uniq str / 1M formulas | `81.6` / `9.3` / `110.6` / `825.0 MB` WASM | `80.3` / `8.1` / `79.3` / `512.1 MB` est | baseline |
| **B10** — Cold first visit (`20 Mbit/s`, `50 ms` RTT, 2-RTT TLS) | **`475 ms`** shell / **`878 ms`** first screen | — | `≤ 1.5 s` / `≤ 2.0 s` |
| **1M × 22 Scroll** — 5 screens/s continuous scroll frame work | **`0.71 ms`** `p50` / **`1.01 ms`** `p99` | — | `p99 ≤ 16.7 ms` |
| **Bundle Size** — Core `.wasm` / first-screen JS / total JS (brotli-11) | **`461.8 KB`** / **`153.3 KB`** / **`157.5 KB`** | — | `≤ 1.2 MB` / `≤ 165 KB` / `≤ 220 KB` |

## Measured Collaboration (M2a)

Benchmark B9 delivers remote commits to a client that still has local edits pending, on a 100,000-row document with 100,000 formulas. Same machine as above (pinned to P-cores, Windows 11, Google Chrome `158.0.8127.36`); `p50` / `p95`; raw reports in [`bench/results/`](bench/results/) (`*-sync.json`):

| Pending when the remote commit arrives | Sync machine (Chrome WASM) | Worker handles the frame (Chrome WASM) | Sync machine (native) |
|---|---:|---:|---:|
| **B9a** — 1,000 single-cell edits | **`0.87`** / `1.24 ms` | `54.6` / `60.6 ms` | `0.81` / `0.90 ms` |
| **B9b** — one 100,000-cell edit + 9 single cells | `49.6` / `52.3 ms` | `104.1` / `119.4 ms` | `45.3` / `47.2 ms` |
| Nothing pending | `0.005` / `0.01 ms` | `0.005` / `0.01 ms` | `0.002` / `0.002 ms` |

Replaying pending edits in place instead of copying the confirmed workbook took B9a's sync-machine time from `27.5 ms` to `0.87 ms` in Chrome. After such a commit the worker still rebuilds the formula engine and recalculates every formula in 4 ms slices (about one second for 100,000 formulas in Chrome); recalculating only the affected cells is the next step.

The simulation passes 100,000 seeds, and one CPU-hour of `fuzz_sync` (98.4 million executions on 16 threads) found no crash. Two browsers editing one workbook live, offline edits merging with a concurrent row insert, and the takeover of a closed tab's unsent edits are end-to-end tests in Chromium, Firefox and WebKit. Brotli-11 bundle at this milestone: core `.wasm` `549.6 KB`, first-screen JS `157.1 KB`, all JS `168.1 KB` (budgets `1.2 MB` / `165 KB` / `220 KB`).

## Development

Requires [rustup](https://rustup.rs) (installs the pinned Rust `1.100.0` toolchain automatically), Node.js 26, and pnpm; the collaborative server's database and the full-stack run-through also need Docker. All tasks go through `cargo xtask`:

```sh
cargo xtask setup --browsers        # install pinned tools into .tools/, JS deps, and Playwright browsers
cargo xtask dev                     # start local Vite dev server on http://localhost:5273
cargo xtask check                   # formatting, clippy (native + wasm32 + wasm32v1-none), compat hashes, codegen, biome, tsc
cargo xtask test                    # native nextest, wasm32 kernel tests, conformance, property tests, and Vitest
cargo xtask diff --count 10000      # differential formula testing against ironcalc_base & formualizer-eval
cargo xtask fuzz --cpu-seconds 3600 # 6-target fuzzing campaign (parser, snapshot, wal, ops, eval, sync); --target picks one
cargo xtask bench --affinity FFFF   # run and archive native + Chrome WASM engine benchmarks (B1-B8, B11, B12); --suite b9 for sync
cargo xtask sim --seeds 100000      # deterministic multi-client sync simulation with fault injection
cargo xtask ci                      # full pre-push verification (check, test, server + sim, deny, release build + budget, 3-browser E2E)
cargo xtask db up                   # development PostgreSQL 18 in Docker on 127.0.0.1:5434
cargo xtask serve                   # production build + collaborative server with a dev sign-in on http://127.0.0.1:3200
cargo xtask stack                   # release image + PostgreSQL via infra/compose.yml: sync, server SIGKILL, DB restart
```

Run `cargo xtask` with no arguments to see all available commands.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

## Contributing

Bug reports and questions are welcome through issues. Pull requests are not being accepted at this stage.
