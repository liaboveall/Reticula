# Reticula

A local-first spreadsheet for the browser backed by an identity-addressed `#![no_std]` Rust calculation kernel compiled to WebAssembly, with crash-safe OPFS persistence, offline Service Worker support, on-demand `.xlsx` interoperability, and real-time collaboration through a server-ordered sync protocol that keeps editing offline and merges on reconnect.

## Architecture

Reticula splits rendering, calculation, wire persistence, `.xlsx` interop, and local storage cleanly across crates and threads:

- **Kernel (`crates/core`, `crates/formula`, `crates/wire`, `crates/sync`)** — `#![no_std]` + `alloc` Rust crates verified against `wasm32v1-none`:
  - **64-bit stable row and column identities** (`8-bit epoch | 24-bit replica | 32-bit counter`) stored on run-encoded axes with tombstone counts, paired with `4,096`-row adaptive `Empty` / `Sparse` / `Dense` Copy-on-Write (`Arc`) columnar blocks, lazy atomic content digests, and 8-byte NaN-boxed cell values.
  - **Multi-sheet, structural, and content operations** — Reversible sheet operations (`AddSheet`, `DeleteSheet`, `RenameSheet`, `ReorderSheet`), row/column moves with anchor handoff, multi-key sorting (`Op::PermuteContent` with collaborative row-ID remapping), cut-paste cell moves (`Op::MoveCells` with reference tracking), two-level (workbook + sheet-local) named ranges, worker-local AutoFilter projections (`VisibleRowProjection`), sparse row-height/column-width overrides, freeze panes, layered cell/row/column styles (`StyleTable`), constant-size `FillFormula` templates, and RFC 4180 CSV/TSV import/export.
  - **Formula engine & language service** — Pratt parser with error recovery, coordinate-to-ID binder (`BoundFormula`), Excel-compatible 15-significant-digit comparisons/rounding and outermost cancellation snapping (verified against 22,968 recorded Excel 16.0 cases), dynamic-array spilling with symmetric `#SPILL!` conflict resolution and `#` spill-range operator alongside legacy `@` implicit intersection, **193 built-in functions (200 catalog entries)** across math/trig, logic, info, text, lookup/reference (`XLOOKUP`, `XMATCH`, `FILTER`, `SORT`, `SORTBY`, `UNIQUE`, `SEQUENCE`), `LET`/`LAMBDA`/`MAP`/`REDUCE`/`SCAN`/`BYROW`/`BYCOL`/`MAKEARRAY`, date/time, statistics, financial (`NPV`, `IRR`, `XIRR`, `XNPV`, `PMT`, `PV`, `FV`, `RATE`, `NPER`, `SLN`, `SYD`, `DDB`), engineering/bit, AutoFilter-aware `SUBTOTAL`/`AGGREGATE`, and volatile functions; anchor-ordered range dependency graph, non-recursive 4 ms time-sliced evaluator with `#CYCLE!` detection, batch Find & Replace (regex, formula validation, atomic undo), and single-pass formula analysis (syntax tokens, diagnostics, completions, signature help, colored reference spans, `F4` absolute/relative cycling).
  - **Sync state machines (`crates/sync`)** — Pure `ClientSyncMachine` and `ServerDocMachine` with no I/O (`SYNC_PROTOCOL_VERSION = 2`): the server orders transactions; a client keeps its unconfirmed ones in a pending queue and, when a remote commit arrives, rewinds them, applies the commit (including `CommittedPermute` row remaps for concurrent sorts), and replays them in place. Per-replica high watermarks make retries idempotent and detect lost tails, log epochs keep replica IDs unique across restores, rejected transactions are saved as a separate local workbook, and a versioned 192-bit `StateDigest` (128-bit additive content digest + 64-bit axis-order digest, formula caches and local AutoFilter view excluded) catches divergence. Undo in a shared workbook uses conditional writes (`SetCellsIf`) that skip cells someone else has changed since.
  - **Frozen Wire v1, v2, and v3 (`crates/wire`)** — Wire v2 adds `SetCellsIf`, version-1 sync frames, and `RTCLWAL2`; Wire v3 adds `PermuteContent`, `MoveCells`, `DefineNamedRange`/`DeleteNamedRange`, `SYNC_PROTOCOL_VERSION = 2`, `RTCLWAL3`, and `RTCLSNAP` sections for named ranges, permute history, and spill metadata (`SeedOrigin` stubs, orphan string compaction, replica watermarks, backward-compatible decoding). Frozen `v1`, `v2`, and `v3` schemas and fixtures under `tests/compat/` are SHA-256 verified on every CI run.
- **On-Demand XLSX Interoperability (`crates/xlsx`, `crates/xlsx-wasm`)** — Compiled to its own lazy `xlsx-pkg` WASM module outside the core engine and pre-cache path: ZIP-bomb-bounded (`<= 64 MiB` raw, `<= 256 MiB` decompressed XML) import/export (`calamine` + `quick-xml` + `rust_xlsxwriter`) plus streaming in-place `patch_xlsx` (`zip` + `quick-xml`), `_xlfn.`/`_xlfn._xlws.` round-trip, 1900/1904 date epoch normalization, preserved unknown/opaque formulas (`PreservedCached` / `Opaque`) retaining cached values until edited, and structured unsupported-feature compatibility reports.
- **Worker, Persistence & Viewport Protocol (`crates/wasm`, `packages/protocol`, `apps/web/src/engine`)** — TypeScript message definitions generated from Rust via `tsify`, zero-copy `ArrayBuffer` viewport pages (`VERSION = 2`, `64 × 16` cells with style IDs, flags, and left-overflow text records), OPFS `FileSystemSyncAccessHandle` double-buffered snapshots (`snapshot-a.bin` / `snapshot-b.bin` + `wal.log` + `state.json`), Web Locks single-instance guard with four-step cross-tab takeover (including hung-worker watchdog and Safari `InvalidStateError` recovery), startup crash sentinel + three-option Safe Mode, and a hand-written Service Worker offline shell with multi-tab coordinated updates.
- **Two renderers, one scene (`packages/grid/src/render`)** — All geometry (quadrants, clips, snapping, text layout, overlays, headers) is computed once as whole device pixels and drawn by either of two backends. **Canvas 2D** draws the first frame and is the fallback everywhere; **WebGPU** takes over after the first screen on hardware adapters in Chromium and Firefox: one quad pipeline, instance buffers kept per eight rows so that scrolling uploads nothing but a uniform, a glyph atlas with four subpixel phases rasterised in each text's own colour, whole-run sprites for scripts that need shaping, a time budget with stand-ins, and recovery from a lost device. Both use grey antialiasing without kerning, so text is the same pixel for pixel. A visual suite compares them in every push (software WebGPU on Linux), on generated cases and a generated test font, with mutation self-checks and golden images.
- **Grid & Editor (`packages/grid`, `apps/web`)** — Zoom from 25% to 200% (whole device pixels per row and column; sizes in the document stay unzoomed), touch panning and pinching with a read-only mode on touch devices, and a virtualized grid with freeze panes, sparse row/column sizing, AutoFilter header chevrons and skipped-row indicators, active spill dashed outline and `SPILL` formula-bar badge, multi-range selection, fill handle with series recognition, rich TSV/HTML clipboard interop, CodeMirror 6 formula bar powered by the Rust language service, formatting ribbon, one command registry behind toolbar, menus, shortcuts and a searchable command palette (`Ctrl+/`), a name box that goes to cells, ranges, sheets and names (`Ctrl+G`), Sort, Filter, Find & Replace and Name Manager dialogs, sheet tab bar, and an always-focused hidden CodeMirror 6 cell editor for CJK input method composition.
- **Deterministic simulation (`crates/sim`)** — Several clients and a server in one seeded, single-threaded world with injected faults (reordering, slow and dropped connections, client and server crashes, OPFS tail loss, epoch restores, permission downgrades, rejects, concurrent sorts, two tabs on one replica) and **eleven invariants**, checked after every step or once the world goes quiet: among them, every client's visible state equals a fresh replay of its pending queue, no acknowledged transaction is lost, concurrent row edits converge across `PermuteContent` sorts, and every replica converges on the server's digest. Failing traces are minimized automatically; 5,000 seeds run on every push and 100,000 nightly. A sixth fuzz target, `fuzz_sync`, feeds arbitrary frames to both machines.
- **Collaboration server (`crates/server`)** — `axum` + `sqlx` on PostgreSQL 18: one actor per open workbook, group commit sized from measurements on the target host (5 ms window, 32 transactions or 256 KiB per flush) and acknowledgement only after durability, snapshots on a disk volume with background retention (`seq = 0`, named-version bases, `5,000`-seq milestones, and latest `5`), GitHub sign-in with PKCE and hashed session tokens, `editor` and `viewer` share links (with optional expiry and single-use tokens), `link_view` anonymous read-only viewing (`replica = 0` sentinel with per-IP and per-room connection limits), workbook rename/delete and `.rtcl` import (validating `<= 2M` cells and `<= 64 MiB` before expansion), live role downgrades/removals (`4003 AccessRevoked`), per-user (`256 MiB`) and global disk quotas, `384 MiB` loaded-room LRU backpressure (`503`/`4008`), token-bucket HTTP rate limits, redacted structured `tracing` logs, loopback-only Prometheus `/metrics`, account deletion with collaborator tombstone attribution, and an OpenAPI description from which the TypeScript client is generated. Integration tests cover the permission matrix across all roles and routes, quota/backpressure limits, and a server killed mid-write that must keep every acknowledged transaction. The same binary serves the precompressed static build. `infra/compose.yml` runs it with PostgreSQL, volumes, and the target host's resource limits, paired with cron-ready backup/restore/monitor scripts and `infra/ops/RUNBOOK.md`.
- **AI Analyst, Draft Branches & Three-Way Selective Merge (`crates/agent`, `apps/web/src/ai/`)** — Isolated `ai_draft` branches backed by a persistent multi-step tool loop (`11` spreadsheet tools including side-effect-free `evaluate_formula` via `FormulaEngine::evaluate_transient_formula`, error precedent auditing via `list_errors`, `SheetCompressor` context compression, and streaming SSE OpenAI-compatible client with `Retry-After` backoff and fail-closed `.env.local` validation):
  - **Three-Way Diff & Static Reference Closure**: Compares `base` (`fork_seq`), `draft`, and live `main` inside the authoritative `DocActor` with volatile cache exclusion, atomic structural hunks, folded `SortHunk`s, explicit `Keep Main` / `Use AI Draft` cell conflict choices, and static `BoundExpr` reference-closure checks that prevent dangling `#REF!` references (or explicitly rewrite them to `BoundRef::Invalid` when opted in) before remapping IDs onto a fresh merge replica via `RemapState`.
  - **Shared Memory Governance & Crash Recovery**: Draft branch actors share the `384 MiB` loaded-room memory budget (with `2×` draft engine leases evicting idle rooms LRU), clamp draft WebSocket viewers to `VIEWER_REPLICA = 0`, broadcast purple `"AI"` cursors (`color_slot = 0x80 | 5`), enforce per-user daily token/run quotas, and recover or cleanly fail interrupted runs on server restart after `log_epoch` and `agent_steps` verification.
  - **10 Pure-Client Interactive Replays & Evaluation Harness**: `apps/web/public/data/ai-replays/` ships 10 deterministic `ReplayBundle` scenarios (lazy-fetched outside JS bundles and `sw.js` precaching) runnable locally in-browser by unauthenticated visitors via the real WASM `FormulaEngine`; `cargo xtask spike s6` and `cargo xtask eval` provide the S6 tool-loop probe and SpreadsheetBench Verified headless dual-path (`patch_xlsx` / `export_xlsx`) evaluation harness.
- **Collaborative client & lazy product shell (`apps/web`, `crates/sim-wasm`)** — Outside the first-screen bundle via `React.lazy`: `/benchmarks` (unauthenticated zero-dependency SVG performance & differential verification dashboard fetching `/data/benchmarks-history.json`), `/workbooks` (owned, shared, local OPFS docs, `.rtcl` / `.xlsx` import, and one-click Convert to Cloud), `/account` (storage + daily AI quota & irreversible account deletion), `/privacy`, and `/sim` (loading `crates/sim-wasm` on demand for 2–3 interactive multi-grid collaborative replicas and a 5-replica seed runner). Inside `/app`, each tab claims its own OPFS replica directory, persists pending transactions before sending, takes over closed tabs' unsent edits, resolves remote cursors by stable row/column IDs, opens an isolated read-only `HistoryPlayer` drawer with sequence scrubbing and named versions, opens the lazy `AiPanel` / `DiffReviewDrawer` with isolated in-memory `DraftPreviewSession`, and locks the grid/formula bar when opened as a viewer.

## Measured Performance

Measured on an Intel Core i9-13980HX (pinned to P-cores `0xFFFF`, 32 GB RAM, GeForce RTX 4080 Laptop, Windows 11, Google Chrome `158.0.8127.37`). Raw reports, with machine, browser and commit, are archived under [`bench/results/`](bench/results/): engine and sync at `d4ee458`, grid, renderers and startup at `9c2544c` (2026-11-25).

| Workload | Chrome WASM | Native (`x86_64`) | Target |
|---|---:|---:|---:|
| **B1** — `1M × 20` (20M cells + 1M formulas) OPFS load → first page | `316.3 ms` (`p50`) | `255.2 ms` | `≤ 2,000 ms` |
| **B11** — `20M + 1M` formulas background CoW OPFS save / 25 ms probe `p95` | `504.5 ms` / `10.2 ms` | `473.5 ms` | `≤ 2,500 ms` / `≤ 16 ms` |
| **B12** — `1M × 50` (50M cells + 100k formulas) OPFS load / 100k recalc | `393.2 ms` / `1.07 ms` | `269.1 ms` / `0.56 ms` | `≤ 3,000 ms` / `≤ 50 ms` |
| **B12** — save / 25 ms probe `p95` / peak WASM memory | `608.6 ms` / `30.9 ms`¹ / `579.5 MB` | — | `≤ 4,000 ms` / `≤ 16 ms` / `≤ 1.2 GiB` |
| **B2** — `=SUM(A1:A1000000)` over 1M rows | `2.19 ms` | `1.58 ms` | — |
| **B3** — `1,000,000` filled formulas, full recalc | `7.5 ms` | `5.48 ms` | `≤ 80 ms` |
| **B4** — `100,000` formulas `=A2*$Z$1` incremental recalc | `0.61 ms` | `0.3 ms` | `≤ 50 ms` |
| **B5** — `1,000,000`-row chain `=C1+B2` | `19.09 ms` | `15.8 ms` | no stack overflow |
| **B6** — `100k` exact `VLOOKUP` over `100k` rows | `3.65 ms` | `1.7 ms` | `≤ 50 ms` |
| **B7a** — Top-row insert, structural formulas / cross-range formulas (1M rows) | `0.56 ms` / `2.04 ms` | `0.45 ms` / `1.59 ms` | `≤ 20 ms` |
| **B8** — WASM memory: 10M numbers / 1M repeated / 1M unique strings / 1M formulas | `81.6` / `9.3` / `110.6` / `25.4 MB` | — | — |
| **B10** — Cold first visit (`20 Mbit/s`, `50 ms` RTT, 2-RTT TLS), shell / first screen | `408 ms` / `1,028 ms` | — | `≤ 1.5 s` / `≤ 2.0 s` |
| **Bundles** (brotli-11) — core / sim / XLSX `.wasm` | `965.9 KB` / `704.8 KB` / `762.5 KB` | — | `≤ 1.2 MB` / `≤ 800 KB` / `≤ 900 KB` |
| **JS** (brotli-11) — editor / landing first screen / WebGPU / Chinese / total | `177.8` / `70.9` / `11.8` / `26.9` / `297.5 KB` | — | `≤ 180` / `80` / `16` / `40` / `300 KB` |

¹ Noisy on this machine: 11 to 37 ms over eight alternating runs of this and the previous release's engine.

### Renderers

Benchmark B13 draws the same sheets with both renderers in one browser, in turns, with the window on a 240 Hz screen. In the app: a 14,000 × 120 sheet at 25% zoom in a 2,560 × 1,300 window, **16,650 cells drawn in every frame**, scrolled at five screens a second for 8 s.

| App, 16,650 cells | Main thread per frame, `p50` / `p99` | Between frames, `p50` | Frames over 16.7 ms |
|---|---:|---:|---:|
| Chrome 158, Canvas 2D | `16.65` / `32.58 ms` | `29.2 ms` | 255 of 255 |
| Chrome 158, **WebGPU** | **`0.46`** / **`1.02 ms`** | **`4.17 ms`** | **0 of 1,919** |
| Firefox 159, Canvas 2D | `41.74` / `51.04 ms` | `41.7 ms` | 185 of 185 |
| Firefox 159, **WebGPU** | **`0.64`** / **`1.44 ms`** | **`4.16 ms`** | **0 of 1,920** |
| Chrome, main thread slowed 4×, **WebGPU** | `2.08` / `5.43 ms` | `4.17 ms` | 0 of 1,892 |

With a thousand cells, Canvas 2D takes the main thread 0.69 to 1.01 times as long as before the renderers shared a scene (`*-ab.json`, alternating runs against `5b46fff`). While a zoom gesture is running at 10,000 cells, WebGPU shows stretched stand-ins for texts that are not ready and replaces them within a few frames. Chrome's and Firefox's adapters were a discrete GPU; integrated and mobile GPUs were not measured.

## Measured Collaboration, Formula & End-to-End Verification

Benchmark B9 delivers remote commits to a client that still has local edits pending, on a 100,000-row document with 100,000 formulas (`d4ee458`; `p50` / `p95`; raw reports in [`bench/results/`](bench/results/)):

| Pending when the remote commit arrives | Sync machine (Chrome WASM) | First frame (Chrome WASM) | Full recalc (Chrome WASM) | Full recalc (native) |
|---|---:|---:|---:|---:|
| **B9a** — 1,000 single-cell edits | `0.7` / `0.81 ms` | `2.11` / `2.48 ms` | `2.63` / `3.11 ms` | `2.27` / `2.42 ms` |
| **B9b** — one 100,000-cell edit + 9 single cells | `30.93` / `32.56 ms` | `69.91` / `73.95 ms` | `73.72` / `77.97 ms` | `67.82` / `72.44 ms` |
| Nothing pending | `0` / `0.01 ms` | `0.01` / `0.01 ms` | `0.01` / `0.01 ms` | `0` / `0 ms` |

- **Differential Formula Verification (`76dcc45`)**: Stratified evaluation against containerized **LibreOffice Calc `26.8.2.2`** (`infra/libreoffice/Dockerfile`) over **`51,250` formulas** (`76,510` compared cells across `195` catalog / `188` unique deterministic functions, `>= 250` samples each) reaches **`97.92%` non-error** (`>= 80%`), **`99.65%` agreement** (`>= 99.5%`), and **`0` unclassified divergences** (the documented LibreOffice discrepancies, `14` clusters of `10` kinds, are published on `/benchmarks`). The `10,000`-formula Rust reference sweep matches `9,837` cases against `ironcalc_base` and `9,772` against `formualizer-eval`.
- **50-Client Compose Load Test (`8a858fd`)**: Under `infra/compose.yml` container limits (`768 MiB` / `1.0` CPU server, `768 MiB` / `0.5` CPU PostgreSQL 18), `cargo xtask loadtest` drives `50` concurrent WebSocket clients at `100 txns/s` (`6,000` measured txns over `1,977,000` cells) at **`10.04 ms`** `p50` / **`35.95 ms`** `p95` loopback visibility (**`85.95 ms`** `p95` with `+50 ms` RTT, target `<= 150 ms`), **`320.2 MiB`** server peak (`41.7%` of limit), `0` disconnects, `0` lost transactions, and `0` `StateDigest` mismatches.
- **Simulation, Compatibility & End-to-End Coverage**: All `100,000` simulation seeds pass across all 11 invariants, frozen compatibility fixtures (`v1`, `v2`, `v3`) pass byte/replay verification on every commit, `cargo xtask drill` validates backup/epoch recovery, and three-browser Playwright E2E suites (Chromium, Firefox, WebKit) verify spills/`#SPILL!`/`#`, two-level named ranges, local AutoFilter + `SUBTOTAL`, Find & Replace, `MoveCells` cut-paste, collaborative concurrent sort (`PermuteContent`), `.xlsx` round-trips including preserved opaque formulas, `/benchmarks`, and M4 AI Analyst workflows (the 10-demo pure-client visitor replay catalog and end-to-end replay apply, isolated AI draft branch previews, three-way concurrent edit conflicts, static reference closure guards, and live merged sync across collaborators).

## Accessibility and languages

- Every function of the editor can be reached with the keyboard alone; `F6` cycles through the parts of the window, `Ctrl+/` (`Cmd+/`) lists and searches all commands with their keys. End-to-end tests walk the flows in three browsers with the mouse forbidden.
- The canvas is hidden from assistive technology. A hidden table mirrors the active cell, and a live region reads the address, contents, formula, errors, spills and ranges as the selection moves; dialogs are native `<dialog>` elements with their own announcements. Focus rings, contrast, reduced motion and forced colours are covered by tests.
- The interface is in English and Simplified Chinese, including function help and engine messages. Function names, error values and formula syntax stay `en-US`.
- **Not verified:** real screen readers (NVDA, VoiceOver, TalkBack), Safari with a keyboard on macOS beyond automation, real phones and tablets, and WebGPU in Safari (which stays on Canvas 2D). Browsing many cells with a screen reader's table commands is not supported: only the active cell is mirrored.

## Development

Requires [rustup](https://rustup.rs) (installs the pinned Rust `1.100.0` toolchain automatically), Node.js 26, and pnpm; the collaborative server's database, containerized LibreOffice diff, and full-stack run-through also need Docker. All tasks go through `cargo xtask`:

```sh
cargo xtask setup --browsers            # install pinned tools into .tools/, JS deps, and Playwright browsers
cargo xtask dev                         # start local Vite dev server on http://localhost:5273
cargo xtask check                       # formatting, clippy (native + wasm32 + wasm32v1-none), compat hashes, codegen, biome, tsc
cargo xtask test                        # native nextest, wasm32 kernel tests, Node xlsx smoke, conformance, property tests, and Vitest
cargo xtask compat                      # verify frozen wire v1/v2/v3 schemas and snapshot/WAL replay fixtures
cargo xtask diff --engine all           # 51k stratified differential check (LibreOffice Calc container + IronCalc + Formualizer)
cargo xtask fuzz --cpu-seconds 3600     # 6-target fuzzing campaign (parser, snapshot, wal, ops, eval, sync); --target picks one
cargo xtask bench --affinity FFFF       # engine benchmarks (B1-B8, B11, B12); --suite b9|render|grid|startup|ab for sync, B13, S1/S2, B10 and regressions
cargo xtask visual                      # compare the two renderers on generated cases (golden images in CI)
cargo xtask sim --seeds 100000          # deterministic multi-client sync simulation with fault injection
cargo xtask ci                          # full pre-push verification (check, test, server + sim, deny, release build + budget, 3-browser E2E)
cargo xtask db up                       # development PostgreSQL 18 in Docker on 127.0.0.1:5434
cargo xtask serve                       # production build + collaborative server with a dev sign-in on http://127.0.0.1:3200
cargo xtask stack                       # release image + PostgreSQL via infra/compose.yml: sync, server SIGKILL, DB restart
cargo xtask drill                       # compose backup/restore drill verifying epoch 0->1 bump and stale-client catch-up
cargo xtask loadtest                    # 50-client / 100 txns/s load test against infra/compose.yml resource limits
cargo xtask eval replays                # regenerate the 10 built-in client AI replay bundles in apps/web/public/data/ai-replays
cargo xtask spike s6 --mock             # S6 multi-step AI tool-loop probe (omit --mock with a local .env.local LLM key)
cargo xtask eval download               # SpreadsheetBench Verified dataset setup & headless eval CLI (`run`/`grade`/`report`)
```

Run `cargo xtask` with no arguments to see all available commands.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

## Contributing

Bug reports and questions are welcome through issues. Pull requests are not being accepted at this stage.
