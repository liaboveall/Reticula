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
- **Collaboration server (`crates/server`)** — `axum` + `sqlx` on PostgreSQL 18: one actor per open workbook, group commit sized from measurements on the target host (5 ms window, 32 transactions or 256 KiB per flush) and acknowledgement only after the transaction is durable, snapshots on a disk volume with background retention (`seq = 0`, named-version bases, `5,000`-seq milestones, and latest `5`), GitHub sign-in with PKCE and hashed session tokens, `editor` and `viewer` share links (with optional expiry and single-use tokens), `link_view` anonymous read-only viewing (`replica = 0` sentinel with per-IP and per-room connection limits), workbook rename/delete and `.rtcl` import (validating `<= 2M` cells and `<= 64 MiB` before expansion), live role downgrades/removals (`4003 AccessRevoked`), per-user (`256 MiB`) and global disk quotas, `384 MiB` loaded-room LRU backpressure (`503`/`4008`), token-bucket HTTP rate limits, redacted structured `tracing` logs, loopback-only Prometheus `/metrics`, account deletion with collaborator tombstone attribution, and an OpenAPI description from which the TypeScript client is generated. Integration tests cover the permission matrix across all roles and routes, quota/backpressure limits, and a server killed mid-write that must keep every acknowledged transaction. The same binary serves the precompressed static build. `infra/compose.yml` runs it with PostgreSQL, volumes, and the target host's resource limits, paired with cron-ready backup/restore/monitor scripts and `infra/ops/RUNBOOK.md`.
- **Collaborative client & lazy product shell (`apps/web`, `crates/sim-wasm`)** — Outside the first-screen bundle via `React.lazy`: `/workbooks` (owned, shared, local OPFS docs, `.rtcl` import, and one-click Convert to Cloud), `/account` (storage quota & irreversible account deletion), `/privacy`, and `/sim` (loading `crates/sim-wasm` on demand for 2–3 interactive multi-grid collaborative replicas and a 5-replica seed runner). Inside `/app`, each tab claims its own OPFS replica directory, persists pending transactions before sending, takes over closed tabs' unsent edits, resolves remote cursors by stable row/column IDs, opens an isolated read-only `HistoryPlayer` drawer with sequence scrubbing and named versions, and locks the grid/formula bar when opened as a viewer.

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

## Measured Collaboration & Load (M2b)

Benchmark B9 delivers remote commits to a client that still has local edits pending, on a 100,000-row document with 100,000 formulas. Same machine as above (pinned to P-cores, Windows 11, Google Chrome `158.0.8127.36`); `p50` / `p95`; raw reports in [`bench/results/`](bench/results/) (`*-sync.json`):

| Pending when the remote commit arrives | Sync machine (Chrome WASM) | First frame (Chrome WASM) | Full recalc (Chrome WASM) | Full recalc (native) |
|---|---:|---:|---:|---:|
| **B9a** — 1,000 single-cell edits | **`0.99`** / `1.22 ms` | **`2.44`** / `2.85 ms` | **`4.95`** / `6.19 ms` | `4.83` / `5.68 ms` |
| **B9b** — one 100,000-cell edit + 9 single cells | `52.8` / `54.7 ms` | `122.2` / `126.9 ms` | `135.5` / `140.6 ms` | `128.0` / `135.9 ms` |
| Nothing pending | `0` / `0.005 ms` | `0.005` / `0.01 ms` | `0.005` / `0.01 ms` | `0.004` / `0.005 ms` |

Maintaining parallel `FormulaEngine` inverse logs on pending edits and tightening `BLOCKED` slice checks (`64` steps per check) replaced full-workbook rebuilds on incoming commits, cutting B9a total time in Chrome from ~1 second to **`4.95 ms`** (`2.44 ms` first frame, `p95` `6.19 ms`).

Under `infra/compose.yml` container limits (`768 MiB` / `1.0` CPU server, `768 MiB` / `0.5` CPU PostgreSQL 18), `cargo xtask loadtest` drives `50` concurrent WebSocket clients at `100 txns/s` (`5s` warmup + `60s` measured = `6,000` txns, `294,000` remote visibility samples) over a `1,977,000`-cell / `62.16 MiB` (`8.31 MiB` compressed) workbook (`*-load.json`):

- **Remote visibility latency**: **`10.04 ms`** `p50` / **`35.95 ms`** `p95` on loopback (**`60.04 ms`** `p50` / **`85.95 ms`** `p95` with `+50 ms` reference RTT, target `p95 <= 150 ms`).
- **Resource & integrity**: server peak **`320.2 MiB`** (`41.7%` of `768 MiB`, target `<= 70%`), PostgreSQL peak `72.3 MiB`, `62` snapshots written / `6` pruned, `0` disconnects, `0` lost transactions, and `0` `StateDigest` mismatches across all 50 clients.

The simulation passes 100,000 seeds, one CPU-hour of `fuzz_sync` (98.4 million executions on 16 threads) found no crash, and `cargo xtask drill` automates backup, epoch increment (`0 -> 1`), and stale-client catch-up across isolated containers. End-to-end tests across Chromium, Firefox, and WebKit cover real-time sync, offline merge, closed-tab takeover, `/workbooks` management + local-to-cloud conversion, anonymous `viewer` links + live role revocations, `HistoryPlayer` scrubbing + named versions, `/account` deletion, and `/sim`. Brotli-11 bundle at M2b: core `.wasm` `563,119 B` (`<= 1,200,000 B`), on-demand simulator `.wasm` `396,864 B` (`<= 800,000 B`), first-screen JS `160,452 B` (`<= 165,000 B`), total JS `185,717 B` (`<= 220,000 B`).

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
cargo xtask drill                   # compose backup/restore drill verifying epoch 0->1 bump and stale-client catch-up
cargo xtask loadtest                # 50-client / 100 txns/s load test against infra/compose.yml resource limits
```

Run `cargo xtask` with no arguments to see all available commands.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

## Contributing

Bug reports and questions are welcome through issues. Pull requests are not being accepted at this stage.
