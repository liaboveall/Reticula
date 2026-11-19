import { BENCHMARKS } from "./generated/benchmarks.ts";

export interface LandingPageProps {
  readonly onOpenApp: () => void;
}

export function LandingPage({ onOpenApp }: LandingPageProps) {
  const startupValue =
    BENCHMARKS.startupFirstScreenP50Ms !== null
      ? `${(BENCHMARKS.startupFirstScreenP50Ms / 1000).toFixed(2)} s`
      : "≤ 2.0 s";
  const startupNote =
    BENCHMARKS.startupFirstScreenP50Ms !== null && BENCHMARKS.startupShellP50Ms !== null
      ? `First screen p50 (shell ${(BENCHMARKS.startupShellP50Ms / 1000).toFixed(2)} s) over 20 Mbit/s · 50 ms RTT`
      : "Cold start target over 20 Mbit/s · 50 ms RTT · 2-RTT TLS";

  const renderNote =
    BENCHMARKS.renderScrollWorkP99Ms !== null
      ? `Main-thread frame work p99 ${BENCHMARKS.renderScrollWorkP99Ms.toFixed(2)} ms during 1M-row scroll`
      : "Zero-DOM Canvas2D viewport grid over 64×16 binary pages";

  return (
    <div className="landing" data-testid="landing-page">
      <nav className="landing-nav">
        <span className="landing-brand">Reticula</span>
        <div className="landing-nav-links">
          <a
            href="https://github.com/liaboveall/Reticula"
            target="_blank"
            rel="noreferrer"
            className="landing-link"
          >
            GitHub
          </a>
          <a
            href="/app"
            className="landing-cta-small"
            onClick={(event) => {
              event.preventDefault();
              onOpenApp();
            }}
          >
            Open Demo →
          </a>
        </div>
      </nav>

      <main className="landing-main">
        <section className="landing-hero">
          <p className="landing-eyebrow">Local-First Rust + WebAssembly Spreadsheet Engine</p>
          <h1>
            One million rows in the browser, backed by an identity-addressed <code>no_std</code>{" "}
            Rust kernel.
          </h1>
          <p className="landing-lead">
            Reticula runs its entire calculation engine inside a dedicated Web Worker: run-encoded
            64-bit row and column axes, Copy-on-Write 8-byte NaN-boxed columnar blocks, 69
            Excel-compatible functions with 15-significant-digit arithmetic, OPFS crash-safe local
            persistence with offline Service Worker support, and a custom Canvas2D grid that never
            touches the DOM per cell.
          </p>
          <div className="landing-actions">
            <a
              href="/app"
              className="landing-cta"
              data-testid="open-demo"
              onClick={(event) => {
                event.preventDefault();
                onOpenApp();
              }}
            >
              Open 1,000,000-Row Live Demo →
            </a>
            <a
              href="https://github.com/liaboveall/Reticula"
              target="_blank"
              rel="noreferrer"
              className="landing-secondary"
            >
              View Source on GitHub
            </a>
          </div>
        </section>

        <section className="landing-metrics" aria-label="Measured benchmarks">
          <div className="metric-card">
            <span className="metric-label">100k Incremental Recalc (B4)</span>
            <strong className="metric-value">{BENCHMARKS.b4Incremental100kP50Ms} ms</strong>
            <span className="metric-detail">
              p50 ({BENCHMARKS.b4Incremental100kP95Ms} ms p95) marking + evaluating 100,000
              dependent formulas in WASM
            </span>
          </div>
          <div className="metric-card">
            <span className="metric-label">Top-Row Insert on 1M Sheet (B7a)</span>
            <strong className="metric-value">{BENCHMARKS.b7aInsertTopMeanUs} µs</strong>
            <span className="metric-detail">
              Mean across 100 insertions on 1M row-local formulas — zero AST or range-index rewrites
            </span>
          </div>
          <div className="metric-card">
            <span className="metric-label">1,000,000-Row SUM (B2)</span>
            <strong className="metric-value">{BENCHMARKS.b2Sum1mP50Ms} ms</strong>
            <span className="metric-detail">
              p50 direct scan across 4,096-row adaptive dense column chunks ({renderNote})
            </span>
          </div>
          <div className="metric-card">
            <span className="metric-label">20M Cells + 1M Formulas Load (B1)</span>
            <strong className="metric-value">
              {BENCHMARKS.b1Load20m1mP50Ms !== null
                ? `${Math.round(BENCHMARKS.b1Load20m1mP50Ms)} ms`
                : "≤ 2.0 s"}
            </strong>
            <span className="metric-detail">
              OPFS v1 snapshot decode + DepGraph + first page (CoW save{" "}
              {BENCHMARKS.b11Save20m1mP50Ms !== null
                ? `${Math.round(BENCHMARKS.b11Save20m1mP50Ms)} ms`
                : "≤ 2.5 s"}
              , probe p95 {BENCHMARKS.b11ProbeP95Ms ?? "—"} ms)
            </span>
          </div>
          <div className="metric-card">
            <span className="metric-label">50M-Cell Upper Limit (B12)</span>
            <strong className="metric-value">
              {BENCHMARKS.b12Load50mP50Ms !== null
                ? `${Math.round(BENCHMARKS.b12Load50mP50Ms)} ms`
                : "≤ 3.0 s"}
            </strong>
            <span className="metric-detail">
              1M×50 load (CoW save{" "}
              {BENCHMARKS.b12Save50mP50Ms !== null
                ? `${Math.round(BENCHMARKS.b12Save50mP50Ms)} ms`
                : "≤ 4.0 s"}
              , 100k recalc {BENCHMARKS.b12Recalc100kP50Ms ?? "—"} ms, peak{" "}
              {BENCHMARKS.b12PeakWasmMiB !== null
                ? `${Math.round(BENCHMARKS.b12PeakWasmMiB)} MiB`
                : "≤ 1.2 GiB"}
              )
            </span>
          </div>
          <div className="metric-card">
            <span className="metric-label">Cold First-Visit Load (B10)</span>
            <strong className="metric-value">{startupValue}</strong>
            <span className="metric-detail">{startupNote}</span>
          </div>
        </section>

        <p className="landing-footnote">
          Measured on {BENCHMARKS.cpu} (pinned P-cores <code>0xFFFF</code>), {BENCHMARKS.browser},
          archived at commit <code>{BENCHMARKS.engineCommit}</code> ({BENCHMARKS.engineDate}).
        </p>

        <section className="landing-arch" aria-label="Architecture data flow">
          <h2>Off-Main-Thread Viewport &amp; Identity Architecture</h2>
          <svg
            viewBox="0 0 860 190"
            className="arch-diagram"
            role="img"
            aria-label="Data flow between the main-thread Canvas2D grid and the dedicated Web Worker Rust WASM engine"
          >
            <rect x="12" y="20" width="276" height="150" rx="8" className="arch-box" />
            <text x="28" y="48" className="arch-title">
              Main Thread (UI &amp; Canvas2D)
            </text>
            <text x="28" y="74" className="arch-item">
              • Virtualized Canvas2D renderer
            </text>
            <text x="28" y="98" className="arch-item">
              • Hidden CodeMirror 6 IME cell editor
            </text>
            <text x="28" y="122" className="arch-item">
              • CodeMirror 6 formula bar &amp; multi-select
            </text>
            <text x="28" y="146" className="arch-item">
              • Freeze panes &amp; 64×16 page cache
            </text>

            <path d="M 296 72 L 548 72" className="arch-arrow" />
            <polygon points="548,67 560,72 548,77" className="arch-arrow-head" />
            <text x="428" y="60" textAnchor="middle" className="arch-edge">
              ID-Addressed Commands (RowId, ColId)
            </text>

            <path d="M 560 124 L 308 124" className="arch-arrow" />
            <polygon points="308,119 296,124 308,129" className="arch-arrow-head" />
            <text x="428" y="146" textAnchor="middle" className="arch-edge">
              Zero-Copy ArrayBuffer Viewport Pages (v2)
            </text>

            <rect x="568" y="20" width="280" height="150" rx="8" className="arch-box" />
            <text x="584" y="48" className="arch-title">
              Dedicated Worker (Rust WASM)
            </text>
            <text x="584" y="74" className="arch-item">
              • Run-encoded 64-bit axes + tombstones
            </text>
            <text x="584" y="98" className="arch-item">
              • CoW 8B NaN-boxed blocks + OPFS/WAL
            </text>
            <text x="584" y="122" className="arch-item">
              • Anchor-ordered DepGraph + lang service
            </text>
            <text x="584" y="146" className="arch-item">
              • Non-recursive 4 ms sliced evaluator
            </text>
          </svg>
        </section>
      </main>
    </div>
  );
}
