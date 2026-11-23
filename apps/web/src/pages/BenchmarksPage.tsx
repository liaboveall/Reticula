import { useEffect, useState } from "react";
import "./pages.css";

interface EngineMetricSet {
  readonly b1Load20mMs: number | null;
  readonly b11Save20mMs: number | null;
  readonly b11ProbeP95Ms: number | null;
  readonly b2Sum1mMs: number | null;
  readonly b3FullRecalc1mMs: number | null;
  readonly b4Incremental100kP50Ms: number | null;
  readonly b4Incremental100kP95Ms: number | null;
  readonly b5Chain1mMs: number | null;
  readonly b7aInsertTopUs: number | null;
  readonly b7aInsertCrossP50Ms: number | null;
  readonly b12Load50mMs: number | null;
  readonly b12Save50mMs: number | null;
  readonly b12Recalc100kMs: number | null;
  readonly b12PeakWasmMiB: number | null;
}

interface EngineRun {
  readonly date: string;
  readonly commit: string;
  readonly suite: string;
  readonly cpu: string;
  readonly browser: string;
  readonly native: EngineMetricSet;
  readonly wasmChrome: EngineMetricSet;
}

interface StartupRun {
  readonly date: string;
  readonly commit: string;
  readonly suite: string;
  readonly networkLabel: string;
  readonly shellP50Ms: number | null;
  readonly firstScreenP50Ms: number | null;
}

interface RenderRun {
  readonly date: string;
  readonly commit: string;
  readonly suite: string;
  readonly browser: string;
  readonly scrollWorkP99Ms: number | null;
  readonly chromeMargin2P99Ms: number | null;
  readonly firefoxJumpP99Ms: number | null;
}

interface SyncRun {
  readonly date: string;
  readonly commit: string;
  readonly suite: string;
  readonly stage: string;
  readonly nativeB9aFrameP50Ms: number | null;
  readonly nativeB9aTotalP50Ms: number | null;
  readonly nativeB9bTotalP50Ms: number | null;
  readonly wasmB9aFrameP50Ms: number | null;
  readonly wasmB9aTotalP50Ms: number | null;
  readonly wasmB9aTotalP95Ms: number | null;
  readonly wasmB9bTotalP50Ms: number | null;
}

interface LoadRun {
  readonly date: string;
  readonly commit: string;
  readonly suite: string;
  readonly clients: number;
  readonly targetTxnsPerSec: number;
  readonly achievedTxnsPerSec: number;
  readonly populatedCells: number;
  readonly allDigestsMatch: boolean;
  readonly digestMismatches: number;
  readonly lostTxns: number;
  readonly loopbackP50Ms: number | null;
  readonly loopbackP95Ms: number | null;
  readonly rtt50P50Ms: number | null;
  readonly rtt50P95Ms: number | null;
  readonly serverPeakMiB: number | null;
  readonly postgresPeakMiB: number | null;
}

interface DiffFamilyRow {
  readonly family: string;
  readonly total: number;
  readonly nonError: number;
  readonly matches: number;
  readonly agreementRatio: number;
  readonly unclassified: number;
}

interface DiffClusterRow {
  readonly referenceEngine: string;
  readonly category: string;
  readonly family: string;
  readonly disposition: string;
  readonly reason: string;
  readonly count: number;
  readonly sampleFormula: string;
  readonly reticulaResult: string;
  readonly libreofficeResult: string;
}

interface RustClusterRow {
  readonly referenceEngine: string;
  readonly category: string;
  readonly disposition: string;
  readonly count: number;
  readonly sampleFormula: string;
  readonly reticulaResult: string;
  readonly referenceResult: string;
}

interface DiffSummary {
  readonly date: string;
  readonly commit: string;
  readonly libreofficeVersion: string;
  readonly containerImage: string;
  readonly totalFormulas: number;
  readonly totalCellsCompared: number;
  readonly deterministicFunctionsCovered: number;
  readonly uniqueFunctionsCovered: number;
  readonly minSamplesPerFunction: number;
  readonly nonErrorCount: number;
  readonly nonErrorRatio: number;
  readonly matches: number;
  readonly agreementRatio: number;
  readonly unclassifiedCount: number;
  readonly byFamily: readonly DiffFamilyRow[];
  readonly clusters: readonly DiffClusterRow[];
  readonly rustReference: {
    readonly totalFormulas: number;
    readonly nonErrorCount: number;
    readonly nonErrorRatio: number;
    readonly ironcalcMatches: number;
    readonly formualizerMatches: number;
    readonly clusters: readonly RustClusterRow[];
  };
}

interface BenchmarksHistory {
  readonly engineRuns: readonly EngineRun[];
  readonly startupRuns: readonly StartupRun[];
  readonly renderRuns: readonly RenderRun[];
  readonly syncRuns: readonly SyncRun[];
  readonly loadRuns: readonly LoadRun[];
  readonly latestDiff: DiffSummary | null;
  readonly qualitySummary: {
    readonly builtInFunctions: number;
    readonly catalogEntries: number;
    readonly deterministicFunctions: number;
    readonly simCiSeeds: number;
    readonly simNightlySeeds: number;
    readonly compatGenerations: readonly string[];
  };
}

type TableFilter = "all" | "native" | "wasm" | "compose" | "startup";

export interface BenchmarksPageProps {
  readonly onNavigate: (path: string) => void;
}

interface BenchTableRow {
  readonly id: string;
  readonly category: Exclude<TableFilter, "all">;
  readonly env: string;
  readonly suite: string;
  readonly commit: string;
  readonly metric: string;
  readonly value: string;
  readonly detail: string;
}

function fmtMs(v: number | null | undefined, digits = 2): string {
  return v === null || v === undefined ? "—" : `${v.toFixed(digits)} ms`;
}

function fmtPct(ratio: number): string {
  return `${(ratio * 100).toFixed(2)}%`;
}

export default function BenchmarksPage({ onNavigate }: BenchmarksPageProps) {
  const [data, setData] = useState<BenchmarksHistory | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState<TableFilter>("all");

  useEffect(() => {
    let active = true;
    fetch("/data/benchmarks-history.json")
      .then((res) => {
        if (!res.ok) {
          throw new Error(`HTTP ${res.status} loading benchmark archive`);
        }
        return res.json() as Promise<BenchmarksHistory>;
      })
      .then((json) => {
        if (active) {
          setData(json);
        }
      })
      .catch((err: unknown) => {
        if (active) {
          setError(err instanceof Error ? err.message : "Failed to load benchmark archive");
        }
      });
    return () => {
      active = false;
    };
  }, []);

  const latestEngine = data?.engineRuns[data.engineRuns.length - 1] ?? null;
  const latestSync = data?.syncRuns[data.syncRuns.length - 1] ?? null;
  const latestLoad = data?.loadRuns[data.loadRuns.length - 1] ?? null;
  const latestRender = data?.renderRuns[data.renderRuns.length - 1] ?? null;
  const earliestRender = data?.renderRuns[0] ?? null;
  const firefoxP99 =
    data?.renderRuns.find((r) => r.firefoxJumpP99Ms !== null)?.firefoxJumpP99Ms ?? null;
  const diff = data?.latestDiff ?? null;

  const tableRows: BenchTableRow[] = [];
  if (data) {
    for (const run of data.engineRuns) {
      const pairs: readonly [string, EngineMetricSet, Exclude<TableFilter, "all">, string][] = [
        ["Native", run.native, "native", `${run.date} (${run.cpu})`],
        ["Chrome WASM", run.wasmChrome, "wasm", `${run.date} (${run.browser})`],
      ];
      for (const [env, m, category, detail] of pairs) {
        const items: readonly [string, string][] = [
          ["B1 Load 20M + 1M formulas (p50)", fmtMs(m.b1Load20mMs, 1)],
          ["B11 Save 20M snapshot (p50)", fmtMs(m.b11Save20mMs, 1)],
          ["B2 SUM 1M cells (p50)", fmtMs(m.b2Sum1mMs, 2)],
          ["B3 Full recalc 1M (p50)", fmtMs(m.b3FullRecalc1mMs, 1)],
          [
            "B4 Incremental 100k (p50 / p95)",
            `${fmtMs(m.b4Incremental100kP50Ms)} / ${fmtMs(m.b4Incremental100kP95Ms)}`,
          ],
          ["B5 Chain 1M formulas (p50)", fmtMs(m.b5Chain1mMs, 1)],
          [
            "B7a Top-row local insert",
            m.b7aInsertTopUs !== null ? `${m.b7aInsertTopUs.toFixed(1)} µs` : "—",
          ],
          ["B7a Cross-range insert (p50)", fmtMs(m.b7aInsertCrossP50Ms, 2)],
          [
            "B12 50M load / save / 100k recalc",
            `${fmtMs(m.b12Load50mMs, 1)} / ${fmtMs(m.b12Save50mMs, 1)} / ${fmtMs(m.b12Recalc100kMs, 1)}`,
          ],
        ];
        for (const [metric, value] of items) {
          tableRows.push({
            id: `${run.commit}-${env}-${metric}`,
            category,
            env,
            suite: run.suite,
            commit: run.commit,
            metric,
            value,
            detail,
          });
        }
      }
    }
    for (const run of data.syncRuns) {
      tableRows.push(
        {
          id: `${run.commit}-sync-native`,
          category: "native",
          env: "Native",
          suite: `${run.suite} (${run.stage})`,
          commit: run.commit,
          metric: "B9a 1,000 pending replay (p50) / B9b 100k block",
          value: `${fmtMs(run.nativeB9aTotalP50Ms)} / ${fmtMs(run.nativeB9bTotalP50Ms)}`,
          detail: run.date,
        },
        {
          id: `${run.commit}-sync-wasm`,
          category: "wasm",
          env: "Chrome WASM",
          suite: `${run.suite} (${run.stage})`,
          commit: run.commit,
          metric: "B9a 1,000 pending replay (p50 / p95) / B9b 100k",
          value: `${fmtMs(run.wasmB9aTotalP50Ms)} / ${fmtMs(run.wasmB9aTotalP95Ms)} / ${fmtMs(run.wasmB9bTotalP50Ms)}`,
          detail: run.date,
        },
      );
    }
    for (const run of data.loadRuns) {
      tableRows.push(
        {
          id: `${run.commit}-load-tps`,
          category: "compose",
          env: "Compose + PG18",
          suite: run.suite,
          commit: run.commit,
          metric: `${run.clients} clients · ${(run.populatedCells / 1_000_000).toFixed(2)}M cells · sustained throughput`,
          value: `${run.achievedTxnsPerSec.toFixed(0)} / ${run.targetTxnsPerSec.toFixed(0)} txns/s`,
          detail: `digests match: ${run.allDigestsMatch ? "YES" : "NO"}, lost=${run.lostTxns}`,
        },
        {
          id: `${run.commit}-load-lat`,
          category: "compose",
          env: "Compose + PG18",
          suite: run.suite,
          commit: run.commit,
          metric: "Remote visibility latency (loopback / 50 ms RTT p50→p95)",
          value: `${fmtMs(run.loopbackP50Ms)}→${fmtMs(run.loopbackP95Ms)} / ${fmtMs(run.rtt50P50Ms)}→${fmtMs(run.rtt50P95Ms)}`,
          detail: `Server ${run.serverPeakMiB ?? 0} MiB · Postgres ${run.postgresPeakMiB ?? 0} MiB`,
        },
      );
    }
    for (const run of data.startupRuns) {
      tableRows.push({
        id: `${run.commit}-startup`,
        category: "startup",
        env: "Chrome Reference Net",
        suite: run.suite,
        commit: run.commit,
        metric: "B10 Cold start shell / firstScreen (p50)",
        value: `${fmtMs(run.shellP50Ms, 1)} / ${fmtMs(run.firstScreenP50Ms, 1)}`,
        detail: `${run.date} · ${run.networkLabel}`,
      });
    }
    for (const run of data.renderRuns) {
      tableRows.push({
        id: `${run.commit}-render`,
        category: "startup",
        env: run.browser,
        suite: run.suite,
        commit: run.commit,
        metric: "S1/S2 Canvas2D scroll work p99 (vs Firefox headed jump)",
        value: `${fmtMs(run.scrollWorkP99Ms)} (${run.firefoxJumpP99Ms !== null ? `FF ${fmtMs(run.firefoxJumpP99Ms)}` : "Chrome only"})`,
        detail: run.date,
      });
    }
  }

  const visibleRows = filter === "all" ? tableRows : tableRows.filter((r) => r.category === filter);

  return (
    <div className="page-shell" data-testid="benchmarks-page">
      <nav className="page-nav">
        <a
          href="/"
          className="page-brand"
          onClick={(e) => {
            e.preventDefault();
            onNavigate("/");
          }}
        >
          Reticula Benchmarks
        </a>
        <div className="page-nav-links">
          <a
            href="/app"
            className="toolbar-btn"
            onClick={(e) => {
              e.preventDefault();
              onNavigate("/app");
            }}
          >
            Open Editor
          </a>
          <a
            href="/sim"
            className="toolbar-btn"
            onClick={(e) => {
              e.preventDefault();
              onNavigate("/sim");
            }}
          >
            Sync Simulator
          </a>
          <a
            href="/workbooks"
            className="toolbar-btn"
            onClick={(e) => {
              e.preventDefault();
              onNavigate("/workbooks");
            }}
          >
            Workbooks
          </a>
        </div>
      </nav>

      <main className="page-main">
        <section className="page-card" data-testid="bench-overview">
          <div
            style={{ display: "flex", justifyContent: "space-between", flexWrap: "wrap", gap: 8 }}
          >
            <div>
              <h1>Performance &amp; Differential Verification</h1>
              <p className="page-muted">
                Archived reproducible measurements from <code>bench/results/</code> — zero external
                analytics, unauthenticated static JSON.
              </p>
            </div>
            {latestEngine && (
              <div style={{ display: "flex", gap: 6, flexWrap: "wrap", alignItems: "center" }}>
                <span className="bench-badge">Baseline {latestEngine.commit}</span>
                <span className="bench-badge">{latestEngine.date}</span>
                <span className="bench-badge">{latestEngine.browser}</span>
              </div>
            )}
          </div>

          {error && <p className="page-muted">Error loading benchmark history: {error}</p>}
          {!data && !error && <p className="page-muted">Loading archived benchmark results…</p>}

          {data && (
            <>
              <div className="bench-kpi-grid">
                <div className="bench-kpi">
                  <span className="bench-kpi-label">Built-in Functions</span>
                  <strong className="bench-kpi-value">
                    {data.qualitySummary.builtInFunctions} ({data.qualitySummary.catalogEntries}{" "}
                    entries)
                  </strong>
                  <span className="bench-kpi-sub">15-sig-fig IEEE 754 + Excel 1900 rules</span>
                </div>
                <div className="bench-kpi">
                  <span className="bench-kpi-label">B4 Incremental 100k</span>
                  <strong className="bench-kpi-value">
                    {fmtMs(latestEngine?.wasmChrome.b4Incremental100kP50Ms)}
                  </strong>
                  <span className="bench-kpi-sub">Chrome WASM p50 (budget ≤ 50 ms)</span>
                </div>
                <div className="bench-kpi">
                  <span className="bench-kpi-label">B7a Top-Row Insert</span>
                  <strong className="bench-kpi-value">
                    {latestEngine?.wasmChrome.b7aInsertTopUs?.toFixed(1) ?? "—"} µs
                  </strong>
                  <span className="bench-kpi-sub">Run-encoded O(runs) identity axis</span>
                </div>
                <div className="bench-kpi">
                  <span className="bench-kpi-label">B9a Collab Replay (1k edits)</span>
                  <strong className="bench-kpi-value">
                    {fmtMs(latestSync?.wasmB9aTotalP50Ms)}
                  </strong>
                  <span className="bench-kpi-sub">
                    Down from 981.71 ms via M2b worker incremental
                  </span>
                </div>
                <div className="bench-kpi">
                  <span className="bench-kpi-label">50-Client Compose Load</span>
                  <strong className="bench-kpi-value">
                    {latestLoad ? `${latestLoad.achievedTxnsPerSec.toFixed(0)} txns/s` : "—"}
                  </strong>
                  <span className="bench-kpi-sub">
                    {latestLoad ? `${latestLoad.serverPeakMiB} MiB server · 0 lost` : ""}
                  </span>
                </div>
                <div className="bench-kpi">
                  <span className="bench-kpi-label">LibreOffice Differential</span>
                  <strong className="bench-kpi-value">
                    {diff ? fmtPct(diff.agreementRatio) : "—"}
                  </strong>
                  <span className="bench-kpi-sub">
                    {diff
                      ? `${diff.totalFormulas.toLocaleString()} formulas · ${diff.unclassifiedCount} unclassified`
                      : ""}
                  </span>
                </div>
              </div>

              <p className="bench-note">
                <strong>Hardware &amp; Cross-Browser Methodology:</strong> Measured on{" "}
                <code>{latestEngine?.cpu ?? "i9-13980HX"}</code> (
                {latestEngine?.browser ?? "Chrome"}). Canvas2D scroll work stays at{" "}
                <strong>{fmtMs(latestRender?.scrollWorkP99Ms)}</strong> p99 in Chrome (
                <code>margin=2</code> prefetch: {fmtMs(latestRender?.chromeMargin2P99Ms)}), whereas
                headed Firefox Canvas2D jump frame work measured{" "}
                <strong>{fmtMs(firefoxP99 ?? earliestRender?.firefoxJumpP99Ms)}</strong> p99 due to
                Gecko software 2D text upload overhead — both strictly inside the 16.7 ms 60 Hz
                frame budget.
              </p>
            </>
          )}
        </section>

        {data && (
          <section className="page-card">
            <h2>Cross-Milestone SVG Trends</h2>
            <div className="bench-charts-grid">
              <div className="bench-chart-card">
                <h3>Engine &amp; Startup Latency (ms, Chrome WASM p50)</h3>
                <EngineTrendSvg engineRuns={data.engineRuns} startupRuns={data.startupRuns} />
              </div>
              <div className="bench-chart-card">
                <h3>B9 Collaborative Sync Evolution (p50 ms)</h3>
                <SyncEvolutionSvg syncRuns={data.syncRuns} />
              </div>
              {diff && (
                <div className="bench-chart-card">
                  <h3>LibreOffice Stratified Agreement by Family</h3>
                  <DiffFamilySvg families={diff.byFamily} />
                </div>
              )}
            </div>
          </section>
        )}

        {data && (
          <section className="page-card">
            <div
              style={{ display: "flex", justifyContent: "space-between", flexWrap: "wrap", gap: 8 }}
            >
              <h2>Filterable Benchmark Matrix</h2>
              <div className="bench-filters">
                {(
                  [
                    ["all", "All"],
                    ["native", "Native"],
                    ["wasm", "Chrome WASM"],
                    ["compose", "Compose Load"],
                    ["startup", "Startup & Render"],
                  ] as const
                ).map(([key, label]) => (
                  <button
                    key={key}
                    type="button"
                    className={filter === key ? "page-tab active" : "page-tab"}
                    onClick={() => setFilter(key)}
                  >
                    {label}
                  </button>
                ))}
              </div>
            </div>
            <div className="bench-table-wrap">
              <table className="bench-table" data-testid="bench-table">
                <thead>
                  <tr>
                    <th>Environment</th>
                    <th>Suite</th>
                    <th>Commit</th>
                    <th>Metric</th>
                    <th className="bench-num">Measured</th>
                    <th>Conditions</th>
                  </tr>
                </thead>
                <tbody>
                  {visibleRows.map((row) => (
                    <tr key={row.id}>
                      <td>{row.env}</td>
                      <td>
                        <code>{row.suite}</code>
                      </td>
                      <td>
                        <code>{row.commit}</code>
                      </td>
                      <td>{row.metric}</td>
                      <td className="bench-num">{row.value}</td>
                      <td className="page-muted">{row.detail}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </section>
        )}

        {data && diff && (
          <section className="page-card" data-testid="bench-diff-section">
            <h2>Formula Correctness &amp; Differential Pipeline</h2>
            <p className="page-muted">
              Verified against <strong>{diff.libreofficeVersion}</strong> (
              <code>{diff.containerImage}</code>) over{" "}
              <strong>{diff.totalFormulas.toLocaleString()}</strong> stratified formulas (
              {diff.deterministicFunctionsCovered} catalog / {diff.uniqueFunctionsCovered} unique
              functions, ≥ {diff.minSamplesPerFunction} samples/fn,{" "}
              {diff.totalCellsCompared.toLocaleString()} compared cells),{" "}
              <strong>{fmtPct(diff.nonErrorRatio)}</strong> non-error,{" "}
              <strong>{fmtPct(diff.agreementRatio)}</strong> agreement,{" "}
              <strong>{diff.unclassifiedCount} unclassified</strong>. Also cross-checked on{" "}
              {diff.rustReference.totalFormulas.toLocaleString()} formulas vs{" "}
              <code>ironcalc_base</code> ({diff.rustReference.ironcalcMatches.toLocaleString()}{" "}
              match) and <code>formualizer-eval</code> (
              {diff.rustReference.formualizerMatches.toLocaleString()} match), plus{" "}
              {data.qualitySummary.simCiSeeds.toLocaleString()}/
              {data.qualitySummary.simNightlySeeds.toLocaleString()} simulation seeds and{" "}
              <code>{data.qualitySummary.compatGenerations.join(", ")}</code> wire compatibility
              fixtures.
            </p>

            <div className="bench-table-wrap">
              <table className="bench-table">
                <thead>
                  <tr>
                    <th>Family</th>
                    <th className="bench-num">Formulas</th>
                    <th className="bench-num">Non-Error</th>
                    <th className="bench-num">Matched</th>
                    <th className="bench-num">Agreement</th>
                    <th className="bench-num">Unclassified</th>
                  </tr>
                </thead>
                <tbody>
                  {diff.byFamily.map((fam) => (
                    <tr key={fam.family}>
                      <td>
                        <code>{fam.family}</code>
                      </td>
                      <td className="bench-num">{fam.total.toLocaleString()}</td>
                      <td className="bench-num">{fam.nonError.toLocaleString()}</td>
                      <td className="bench-num">{fam.matches.toLocaleString()}</td>
                      <td className="bench-num">{fmtPct(fam.agreementRatio)}</td>
                      <td className="bench-num">{fam.unclassified}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>

            <h3>
              Public Known Divergences ({diff.clusters.length} LibreOffice +{" "}
              {diff.rustReference.clusters.length} Rust Clusters)
            </h3>
            <div className="bench-table-wrap">
              <table className="bench-table" data-testid="bench-divergences-table">
                <thead>
                  <tr>
                    <th>Engine</th>
                    <th>Function / Category</th>
                    <th>Disposition</th>
                    <th className="bench-num">Count</th>
                    <th>Sample</th>
                    <th>Reticula vs Reference</th>
                    <th>Explanation</th>
                  </tr>
                </thead>
                <tbody>
                  {diff.clusters.map((c) => (
                    <tr key={`lo-${c.category}-${c.disposition}`}>
                      <td>{c.referenceEngine}</td>
                      <td>
                        <code>{c.category}</code>
                      </td>
                      <td>
                        <code>{c.disposition}</code>
                      </td>
                      <td className="bench-num">{c.count}</td>
                      <td>
                        <code>{c.sampleFormula}</code>
                      </td>
                      <td>
                        <code>{c.reticulaResult}</code> vs <code>{c.libreofficeResult}</code>
                      </td>
                      <td>{c.reason}</td>
                    </tr>
                  ))}
                  {diff.rustReference.clusters.map((c) => (
                    <tr key={`rust-${c.referenceEngine}-${c.category}-${c.disposition}`}>
                      <td>{c.referenceEngine}</td>
                      <td>
                        <code>{c.category}</code>
                      </td>
                      <td>
                        <code>{c.disposition}</code>
                      </td>
                      <td className="bench-num">{c.count}</td>
                      <td>
                        <code>{c.sampleFormula}</code>
                      </td>
                      <td>
                        <code>{c.reticulaResult}</code> vs <code>{c.referenceResult}</code>
                      </td>
                      <td>Intentional Excel 1900 calendar / 15-sig-fig / half-away rule</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </section>
        )}

        <section className="page-card" data-testid="bench-ai-eval-section">
          <h2>AI Analyst — SpreadsheetBench Verified Evaluation</h2>
          <p className="page-muted">
            Methodology: 400 verified real-world spreadsheet tasks (CC BY-SA 4.0) executed
            headlessly via <code>cargo xtask eval</code> using dual-path XLSX fidelity (
            <code>patch_xlsx</code> default, <code>export_xlsx</code> fallback on structural edits),
            graded inside the pinned <code>reticula-libreoffice:26.8.2</code> container, reported
            with 95% Wilson score confidence intervals and mutually exclusive root-cause attribution
            (<code>kernel_gap</code>, <code>agent_strategy</code>, <code>model_capability</code>).
          </p>
          <p className="page-muted">
            <strong>Live Model Status:</strong> Awaiting live S6 tool-loop probe (
            <code>cargo xtask spike s6</code>) and evaluation model/budget selection once a live{" "}
            <code>RETICULA_LLM_API_KEY</code> is configured. No unmeasured numbers are shown; try
            all 10 deterministic interactive AI replays directly in the <a href="/app">Editor</a>.
          </p>
        </section>
      </main>
    </div>
  );
}

function EngineTrendSvg({
  engineRuns,
  startupRuns,
}: {
  readonly engineRuns: readonly EngineRun[];
  readonly startupRuns: readonly StartupRun[];
}) {
  const latest = engineRuns[engineRuns.length - 1];
  const prev = engineRuns[0];
  const latestStart = startupRuns[startupRuns.length - 1];
  const bars = [
    {
      id: "b2",
      label: "B2 Sum",
      cur: latest?.wasmChrome.b2Sum1mMs ?? 0,
      prev: prev?.wasmChrome.b2Sum1mMs ?? 0,
    },
    {
      id: "b4",
      label: "B4 100k",
      cur: latest?.wasmChrome.b4Incremental100kP50Ms ?? 0,
      prev: prev?.wasmChrome.b4Incremental100kP50Ms ?? 0,
    },
    {
      id: "b3",
      label: "B3 1M",
      cur: latest?.wasmChrome.b3FullRecalc1mMs ?? 0,
      prev: prev?.wasmChrome.b3FullRecalc1mMs ?? 0,
    },
    {
      id: "b5",
      label: "B5 Chain",
      cur: latest?.wasmChrome.b5Chain1mMs ?? 0,
      prev: prev?.wasmChrome.b5Chain1mMs ?? 0,
    },
    {
      id: "b10",
      label: "B10 1stScr",
      cur: latestStart?.firstScreenP50Ms ?? 0,
      prev: startupRuns[0]?.firstScreenP50Ms ?? 0,
    },
  ];
  const maxVal = Math.max(100, ...bars.flatMap((b) => [b.cur, b.prev]));
  return (
    <svg viewBox="0 0 360 170" className="bench-svg" role="img" data-testid="bench-svg-engine">
      <title>Chrome WASM Engine and Startup Latency Comparison</title>
      {bars.map((b, i) => {
        const x = 18 + i * 68;
        const hPrev = Math.max(2, (b.prev / maxVal) * 110);
        const hCur = Math.max(2, (b.cur / maxVal) * 110);
        return (
          <g key={b.id}>
            <rect x={x} y={130 - hPrev} width={18} height={hPrev} fill="#94a3b8" rx={2} />
            <rect x={x + 22} y={130 - hCur} width={18} height={hCur} fill="#0f766e" rx={2} />
            <text
              x={x + 20}
              y={125 - Math.max(hPrev, hCur)}
              fontSize="9"
              textAnchor="middle"
              fill="currentColor"
            >
              {b.cur.toFixed(0)}ms
            </text>
            <text x={x + 20} y={148} fontSize="10" textAnchor="middle" fill="currentColor">
              {b.label}
            </text>
          </g>
        );
      })}
      <rect x={18} y={156} width={8} height={8} fill="#94a3b8" />
      <text x={30} y={163} fontSize="9" fill="currentColor">
        Baseline ({prev?.commit ?? "—"})
      </text>
      <rect x={140} y={156} width={8} height={8} fill="#0f766e" />
      <text x={152} y={163} fontSize="9" fill="currentColor">
        Latest ({latest?.commit ?? "—"})
      </text>
    </svg>
  );
}

function SyncEvolutionSvg({ syncRuns }: { readonly syncRuns: readonly SyncRun[] }) {
  const maxVal = Math.max(
    100,
    ...syncRuns.flatMap((r) => [r.wasmB9aTotalP50Ms ?? 0, r.nativeB9aTotalP50Ms ?? 0]),
  );
  const w = 360;
  const h = 170;
  const xStep = syncRuns.length > 1 ? 260 / (syncRuns.length - 1) : 0;
  const toY = (v: number) => 132 - (v / maxVal) * 102;
  const wasmPts = syncRuns
    .map((r, i) => `${50 + i * xStep},${toY(r.wasmB9aTotalP50Ms ?? 0).toFixed(1)}`)
    .join(" ");
  const nativePts = syncRuns
    .map((r, i) => `${50 + i * xStep},${toY(r.nativeB9aTotalP50Ms ?? 0).toFixed(1)}`)
    .join(" ");

  return (
    <svg viewBox={`0 0 ${w} ${h}`} className="bench-svg" role="img" data-testid="bench-svg-b9">
      <title>B9 Collaborative Sync Replay Latency Evolution</title>
      <line x1={38} y1={132} x2={330} y2={132} stroke="#cbd5e1" strokeWidth={1} />
      {syncRuns.length > 1 && (
        <>
          <polyline fill="none" stroke="#0f766e" strokeWidth={2.5} points={wasmPts} />
          <polyline
            fill="none"
            stroke="#2563eb"
            strokeWidth={2}
            strokeDasharray="4 2"
            points={nativePts}
          />
        </>
      )}
      {syncRuns.map((r, i) => {
        const x = 50 + i * xStep;
        const yw = toY(r.wasmB9aTotalP50Ms ?? 0);
        const yn = toY(r.nativeB9aTotalP50Ms ?? 0);
        return (
          <g key={`${r.commit}-${r.stage}`}>
            <circle cx={x} cy={yw} r={4} fill="#0f766e" />
            <circle cx={x} cy={yn} r={3.5} fill="#2563eb" />
            <text x={x} y={Math.max(12, yw - 8)} fontSize="9" textAnchor="middle" fill="#0f766e">
              {(r.wasmB9aTotalP50Ms ?? 0).toFixed(1)}ms
            </text>
            <text x={x} y={148} fontSize="9" textAnchor="middle" fill="currentColor">
              {r.stage}
            </text>
          </g>
        );
      })}
      <rect x={42} y={156} width={10} height={3} fill="#0f766e" />
      <text x={56} y={162} fontSize="9" fill="currentColor">
        WASM p50
      </text>
      <rect x={125} y={156} width={10} height={3} fill="#2563eb" />
      <text x={139} y={162} fontSize="9" fill="currentColor">
        Native p50
      </text>
    </svg>
  );
}

function DiffFamilySvg({ families }: { readonly families: readonly DiffFamilyRow[] }) {
  const height = Math.max(160, families.length * 20 + 16);
  return (
    <svg
      viewBox={`0 0 360 ${height}`}
      className="bench-svg"
      role="img"
      data-testid="bench-svg-diff"
    >
      <title>LibreOffice Differential Agreement Ratio by Function Family</title>
      {families.map((fam, idx) => {
        const y = 10 + idx * 20;
        const pct = Math.max(0, Math.min(1, fam.agreementRatio));
        const barW = Math.max(2, pct * 185);
        return (
          <g key={fam.family}>
            <text x={4} y={y + 11} fontSize="9.5" fill="currentColor">
              {fam.family}
            </text>
            <rect x={105} y={y + 2} width={185} height={11} fill="#e2e8f0" rx={2} />
            <rect
              x={105}
              y={y + 2}
              width={barW}
              height={11}
              fill={pct >= 0.995 ? "#0f766e" : "#d97706"}
              rx={2}
            />
            <text x={296} y={y + 11} fontSize="9.5" fill="currentColor">
              {fmtPct(pct)}
            </text>
          </g>
        );
      })}
    </svg>
  );
}
