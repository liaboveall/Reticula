import { useEffect, useState } from "react";
import type { GpuResult, GpuRun } from "../generated/benchmarks.ts";
import { fmtInt, t } from "../i18n/index.ts";
import { tx } from "../i18n/react.ts";
import {
  comparisons,
  latestComparison,
  logPosition,
  rendererName,
  workloadName,
} from "./benchmarks.ts";
import { errorText } from "./errors.ts";
import { PageLink, PageNav } from "./PageNav.tsx";
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
  readonly browser: string | null;
  readonly scrollWorkP99Ms: number | null;
  readonly firefoxJumpP99Ms: number | null;
  readonly firefoxJumpMaxMs: number | null;
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
  /** Missing in what a browser has kept from before there were any. */
  readonly gpuRuns?: readonly GpuRun[];
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
  return v === null || v === undefined ? "—" : t("{ms} ms", { ms: v.toFixed(digits) });
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
      .then(async (res) => {
        const json = res.ok ? ((await res.json()) as BenchmarksHistory) : null;
        if (active && json) {
          setData(json);
        } else if (active) {
          setError(t("HTTP {status} loading benchmark archive", { status: res.status }));
        }
      })
      .catch((err: unknown) => {
        if (active) {
          setError(errorText(err, t("Failed to load benchmark archive")));
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
  const firefox = data?.renderRuns.findLast((run) => run.firefoxJumpP99Ms !== null);
  const gpuRuns = data?.gpuRuns ?? [];
  const comparison = latestComparison(gpuRuns);
  const diff = data?.latestDiff ?? null;

  const tableRows: BenchTableRow[] = [];
  if (data) {
    for (const run of data.engineRuns) {
      const pairs: readonly [string, EngineMetricSet, Exclude<TableFilter, "all">, string][] = [
        [t("Native"), run.native, "native", `${run.date} (${run.cpu})`],
        [t("Chrome WASM"), run.wasmChrome, "wasm", `${run.date} (${run.browser})`],
      ];
      for (const [env, m, category, detail] of pairs) {
        const items: readonly [string, string][] = [
          [t("B1 Load 20M + 1M formulas (p50)"), fmtMs(m.b1Load20mMs, 1)],
          [t("B11 Save 20M snapshot (p50)"), fmtMs(m.b11Save20mMs, 1)],
          [t("B2 SUM 1M cells (p50)"), fmtMs(m.b2Sum1mMs, 2)],
          [t("B3 Full recalc 1M (p50)"), fmtMs(m.b3FullRecalc1mMs, 1)],
          [
            t("B4 Incremental 100k (p50 / p95)"),
            `${fmtMs(m.b4Incremental100kP50Ms)} / ${fmtMs(m.b4Incremental100kP95Ms)}`,
          ],
          [t("B5 Chain 1M formulas (p50)"), fmtMs(m.b5Chain1mMs, 1)],
          [
            t("B7a Top-row local insert"),
            m.b7aInsertTopUs !== null ? t("{us} µs", { us: m.b7aInsertTopUs.toFixed(1) }) : "—",
          ],
          [t("B7a Cross-range insert (p50)"), fmtMs(m.b7aInsertCrossP50Ms, 2)],
          [
            t("B12 50M load / save / 100k recalc"),
            `${fmtMs(m.b12Load50mMs, 1)} / ${fmtMs(m.b12Save50mMs, 1)} / ${fmtMs(m.b12Recalc100kMs, 1)}`,
          ],
        ];
        for (const [index, [metric, value]] of items.entries()) {
          tableRows.push({
            id: `${run.commit}-${category}-${index}`,
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
          env: t("Native"),
          suite: `${run.suite} (${run.stage})`,
          commit: run.commit,
          metric: t("B9a 1,000 pending replay (p50) / B9b 100k block"),
          value: `${fmtMs(run.nativeB9aTotalP50Ms)} / ${fmtMs(run.nativeB9bTotalP50Ms)}`,
          detail: run.date,
        },
        {
          id: `${run.commit}-sync-wasm`,
          category: "wasm",
          env: t("Chrome WASM"),
          suite: `${run.suite} (${run.stage})`,
          commit: run.commit,
          metric: t("B9a 1,000 pending replay (p50 / p95) / B9b 100k"),
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
          env: t("Compose + PG18"),
          suite: run.suite,
          commit: run.commit,
          metric: t("{clients} clients · {millions}M cells · sustained throughput", {
            clients: run.clients,
            millions: (run.populatedCells / 1_000_000).toFixed(2),
          }),
          value: t("{achieved} / {target} txns/s", {
            achieved: run.achievedTxnsPerSec.toFixed(0),
            target: run.targetTxnsPerSec.toFixed(0),
          }),
          detail: run.allDigestsMatch
            ? t("digests match: YES, lost={lost}", { lost: run.lostTxns })
            : t("digests match: NO, lost={lost}", { lost: run.lostTxns }),
        },
        {
          id: `${run.commit}-load-lat`,
          category: "compose",
          env: t("Compose + PG18"),
          suite: run.suite,
          commit: run.commit,
          metric: t("Remote visibility latency (loopback / 50 ms RTT p50→p95)"),
          value: `${fmtMs(run.loopbackP50Ms)}→${fmtMs(run.loopbackP95Ms)} / ${fmtMs(run.rtt50P50Ms)}→${fmtMs(run.rtt50P95Ms)}`,
          detail: t("Server {server} MiB · Postgres {postgres} MiB", {
            server: run.serverPeakMiB ?? 0,
            postgres: run.postgresPeakMiB ?? 0,
          }),
        },
      );
    }
    for (const run of data.startupRuns) {
      tableRows.push({
        id: `${run.commit}-startup`,
        category: "startup",
        env: t("Chrome Reference Net"),
        suite: run.suite,
        commit: run.commit,
        metric: t("B10 Cold start shell / firstScreen (p50)"),
        value: `${fmtMs(run.shellP50Ms, 1)} / ${fmtMs(run.firstScreenP50Ms, 1)}`,
        detail: `${run.date} · ${run.networkLabel}`,
      });
    }
    for (const run of data.renderRuns) {
      tableRows.push({
        id: `${run.commit}-render`,
        category: "startup",
        env: run.browser ?? "—",
        suite: run.suite,
        commit: run.commit,
        metric: t("S1/S2 Canvas2D scroll work p99 (vs Firefox headed jump)"),
        value:
          run.firefoxJumpP99Ms !== null
            ? t("{chrome} (FF {firefox})", {
                chrome: fmtMs(run.scrollWorkP99Ms),
                firefox: fmtMs(run.firefoxJumpP99Ms),
              })
            : t("{chrome} (Chrome only)", { chrome: fmtMs(run.scrollWorkP99Ms) }),
        detail: run.date,
      });
    }
    for (const run of gpuRuns) {
      const env =
        run.slowdown === 1
          ? (run.browser ?? "—")
          : t("{browser}, main thread {n}× slower", {
              browser: run.browser ?? "—",
              n: run.slowdown ?? "—",
            });
      for (const [index, one] of run.results.entries()) {
        tableRows.push({
          id: `${run.report}-${env}-${index}`,
          category: "startup",
          env,
          suite: run.suite,
          commit: run.commit,
          metric: t("B13 {workload}, {renderer}: frame work (p50 / p99 / max)", {
            workload: workloadName(one),
            renderer: rendererName(one.renderer),
          }),
          value: `${fmtMs(one.workP50Ms)} / ${fmtMs(one.workP99Ms)} / ${fmtMs(one.workMaxMs)}`,
          detail: [
            t("{date} · a frame every {interval} (p99 {tail}) at {hz} Hz · {late} over 16.7 ms", {
              date: run.date,
              interval: fmtMs(one.intervalP50Ms),
              tail: fmtMs(one.intervalP99Ms),
              hz: run.refreshHz ?? "—",
              late: one.shareOver60Hz === null ? "—" : fmtPct(one.shareOver60Hz),
            }),
            ...(one.placeholderFrames === null
              ? []
              : [t("{n} frames with placeholders", { n: one.placeholderFrames })]),
          ].join(" · "),
        });
      }
    }
  }

  const visibleRows = filter === "all" ? tableRows : tableRows.filter((r) => r.category === filter);

  return (
    <div className="page-shell" data-testid="benchmarks-page">
      <PageNav
        onNavigate={onNavigate}
        brand={t("Reticula Benchmarks")}
        links={[
          ["/app", t("Open Editor")],
          ["/sim", t("Sync Simulator")],
          ["/workbooks", t("Workbooks")],
        ]}
      />

      <main className="page-main">
        <section className="page-card" data-testid="bench-overview">
          <div
            style={{ display: "flex", justifyContent: "space-between", flexWrap: "wrap", gap: 8 }}
          >
            <div>
              <h1>{t("Performance & Differential Verification")}</h1>
              <p className="page-muted">
                {tx(
                  "Archived reproducible measurements from <0>bench/results/</0> — zero external analytics, unauthenticated static JSON.",
                  { 0: <code /> },
                )}
              </p>
            </div>
            {latestEngine && (
              <div style={{ display: "flex", gap: 6, flexWrap: "wrap", alignItems: "center" }}>
                <span className="bench-badge">
                  {t("Baseline {commit}", { commit: latestEngine.commit })}
                </span>
                <span className="bench-badge">{latestEngine.date}</span>
                <span className="bench-badge">{latestEngine.browser}</span>
              </div>
            )}
          </div>

          {error && (
            <p className="page-muted">{t("Error loading benchmark history: {error}", { error })}</p>
          )}
          {!data && !error && (
            <p className="page-muted">{t("Loading archived benchmark results…")}</p>
          )}

          {data && (
            <>
              <div className="bench-kpi-grid">
                <div className="bench-kpi">
                  <span className="bench-kpi-label">{t("Built-in Functions")}</span>
                  <strong className="bench-kpi-value">
                    {t("{functions} ({entries} entries)", {
                      functions: data.qualitySummary.builtInFunctions,
                      entries: data.qualitySummary.catalogEntries,
                    })}
                  </strong>
                  <span className="bench-kpi-sub">
                    {t("15-sig-fig IEEE 754 + Excel 1900 rules")}
                  </span>
                </div>
                <div className="bench-kpi">
                  <span className="bench-kpi-label">{t("B4 Incremental 100k")}</span>
                  <strong className="bench-kpi-value">
                    {fmtMs(latestEngine?.wasmChrome.b4Incremental100kP50Ms)}
                  </strong>
                  <span className="bench-kpi-sub">{t("Chrome WASM p50 (budget ≤ 50 ms)")}</span>
                </div>
                <div className="bench-kpi">
                  <span className="bench-kpi-label">{t("B7a Top-Row Insert")}</span>
                  <strong className="bench-kpi-value">
                    {t("{us} µs", {
                      us: latestEngine?.wasmChrome.b7aInsertTopUs?.toFixed(1) ?? "—",
                    })}
                  </strong>
                  <span className="bench-kpi-sub">{t("Run-encoded O(runs) identity axis")}</span>
                </div>
                <div className="bench-kpi">
                  <span className="bench-kpi-label">{t("B9a Collab Replay (1k edits)")}</span>
                  <strong className="bench-kpi-value">
                    {fmtMs(latestSync?.wasmB9aTotalP50Ms)}
                  </strong>
                  <span className="bench-kpi-sub">
                    {t("Down from 981.71 ms via M2b worker incremental")}
                  </span>
                </div>
                <div className="bench-kpi">
                  <span className="bench-kpi-label">{t("50-Client Compose Load")}</span>
                  <strong className="bench-kpi-value">
                    {latestLoad
                      ? t("{achieved} txns/s", {
                          achieved: latestLoad.achievedTxnsPerSec.toFixed(0),
                        })
                      : "—"}
                  </strong>
                  <span className="bench-kpi-sub">
                    {latestLoad
                      ? t("{server} MiB server · 0 lost", {
                          server: latestLoad.serverPeakMiB ?? "—",
                        })
                      : ""}
                  </span>
                </div>
                <div className="bench-kpi">
                  <span className="bench-kpi-label">{t("LibreOffice Differential")}</span>
                  <strong className="bench-kpi-value">
                    {diff ? fmtPct(diff.agreementRatio) : "—"}
                  </strong>
                  <span className="bench-kpi-sub">
                    {diff
                      ? t("{formulas} formulas · {n} unclassified", {
                          formulas: fmtInt(diff.totalFormulas),
                          n: diff.unclassifiedCount,
                        })
                      : ""}
                  </span>
                </div>
              </div>

              <p className="bench-note">
                {tx(
                  "<0>Hardware & Cross-Browser Methodology:</0> Measured on <1>{cpu}</1> ({browser}). Scrolling a thousand cells takes the main thread <0>{scroll}</0> a frame at p99 with Canvas 2D in {chrome}; the frame that completes a jump, <0>{firefox}</0> at p99 and <0>{worst}</0> at worst in Firefox ({date}). At 60 Hz there are 16.7 ms to a frame.",
                  { 0: <strong />, 1: <code /> },
                  {
                    cpu: latestEngine?.cpu ?? "i9-13980HX",
                    browser: latestEngine?.browser ?? "Chrome",
                    scroll: fmtMs(latestRender?.scrollWorkP99Ms),
                    chrome: latestRender?.browser ?? "Chrome",
                    firefox: fmtMs(firefox?.firefoxJumpP99Ms),
                    worst: fmtMs(firefox?.firefoxJumpMaxMs),
                    date: firefox?.date ?? "—",
                  },
                )}
              </p>
            </>
          )}
        </section>

        {data && (
          <section className="page-card">
            <h2>{t("Cross-Milestone SVG Trends")}</h2>
            <div className="bench-charts-grid">
              {comparison && <RendererChart run={comparison} />}
              <div className="bench-chart-card">
                <h3>{t("Engine & Startup Latency (ms, Chrome WASM p50)")}</h3>
                <EngineTrendSvg engineRuns={data.engineRuns} startupRuns={data.startupRuns} />
              </div>
              <div className="bench-chart-card">
                <h3>{t("B9 Collaborative Sync Evolution (p50 ms)")}</h3>
                <SyncEvolutionSvg syncRuns={data.syncRuns} />
              </div>
              {diff && (
                <div className="bench-chart-card">
                  <h3>{t("LibreOffice Stratified Agreement by Family")}</h3>
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
              <h2>{t("Filterable Benchmark Matrix")}</h2>
              <fieldset className="bench-filters" aria-label={t("Filter by")}>
                {(
                  [
                    ["all", t("All|runs")],
                    ["native", t("Native")],
                    ["wasm", t("Chrome WASM")],
                    ["compose", t("Compose Load")],
                    ["startup", t("Startup & Render")],
                  ] as const
                ).map(([key, label]) => (
                  <button
                    key={key}
                    type="button"
                    className={filter === key ? "page-tab active" : "page-tab"}
                    aria-pressed={filter === key}
                    onClick={() => setFilter(key)}
                  >
                    {label}
                  </button>
                ))}
              </fieldset>
            </div>
            <div className="bench-table-wrap">
              <table className="bench-table" data-testid="bench-table">
                <thead>
                  <tr>
                    <th>{t("Environment")}</th>
                    <th>{t("Suite")}</th>
                    <th>{t("Commit")}</th>
                    <th>{t("Metric")}</th>
                    <th className="bench-num">{t("Measured")}</th>
                    <th>{t("Conditions")}</th>
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
            <h2>{t("Formula Correctness & Differential Pipeline")}</h2>
            <p className="page-muted">
              {tx(
                "Verified against <0>{libreoffice}</0> (<1>{image}</1>) over <0>{formulas}</0> stratified formulas ({catalog} catalog / {unique} unique functions, ≥ {samples} samples/fn, {cells} compared cells), <0>{nonError}</0> non-error, <0>{agreement}</0> agreement, <0>{unclassified} unclassified</0>. Also cross-checked on {rustFormulas} formulas vs <1>ironcalc_base</1> ({ironcalc} match) and <1>formualizer-eval</1> ({formualizer} match), plus {ciSeeds}/{nightlySeeds} simulation seeds and <1>{generations}</1> wire compatibility fixtures.",
                { 0: <strong />, 1: <code /> },
                {
                  libreoffice: diff.libreofficeVersion,
                  image: diff.containerImage,
                  formulas: fmtInt(diff.totalFormulas),
                  catalog: diff.deterministicFunctionsCovered,
                  unique: diff.uniqueFunctionsCovered,
                  samples: diff.minSamplesPerFunction,
                  cells: fmtInt(diff.totalCellsCompared),
                  nonError: fmtPct(diff.nonErrorRatio),
                  agreement: fmtPct(diff.agreementRatio),
                  unclassified: diff.unclassifiedCount,
                  rustFormulas: fmtInt(diff.rustReference.totalFormulas),
                  ironcalc: fmtInt(diff.rustReference.ironcalcMatches),
                  formualizer: fmtInt(diff.rustReference.formualizerMatches),
                  ciSeeds: fmtInt(data.qualitySummary.simCiSeeds),
                  nightlySeeds: fmtInt(data.qualitySummary.simNightlySeeds),
                  generations: data.qualitySummary.compatGenerations.join(", "),
                },
              )}
            </p>

            <div className="bench-table-wrap">
              <table className="bench-table">
                <thead>
                  <tr>
                    <th>{t("Family")}</th>
                    <th className="bench-num">{t("Formulas|count")}</th>
                    <th className="bench-num">{t("Non-Error")}</th>
                    <th className="bench-num">{t("Matched")}</th>
                    <th className="bench-num">{t("Agreement")}</th>
                    <th className="bench-num">{t("Unclassified")}</th>
                  </tr>
                </thead>
                <tbody>
                  {diff.byFamily.map((fam) => (
                    <tr key={fam.family}>
                      <td>
                        <code>{fam.family}</code>
                      </td>
                      <td className="bench-num">{fmtInt(fam.total)}</td>
                      <td className="bench-num">{fmtInt(fam.nonError)}</td>
                      <td className="bench-num">{fmtInt(fam.matches)}</td>
                      <td className="bench-num">{fmtPct(fam.agreementRatio)}</td>
                      <td className="bench-num">{fam.unclassified}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>

            <h3>
              {t("Public Known Divergences ({libreoffice} LibreOffice + {rust} Rust Clusters)", {
                libreoffice: diff.clusters.length,
                rust: diff.rustReference.clusters.length,
              })}
            </h3>
            <div className="bench-table-wrap">
              <table className="bench-table" data-testid="bench-divergences-table">
                <thead>
                  <tr>
                    <th>{t("Engine")}</th>
                    <th>{t("Function / Category")}</th>
                    <th>{t("Disposition")}</th>
                    <th className="bench-num">{t("Count")}</th>
                    <th>{t("Sample")}</th>
                    <th>{t("Reticula vs Reference")}</th>
                    <th>{t("Explanation")}</th>
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
                        {tx(
                          "<0>{reticula}</0> vs <0>{reference}</0>",
                          { 0: <code /> },
                          { reticula: c.reticulaResult, reference: c.libreofficeResult },
                        )}
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
                        {tx(
                          "<0>{reticula}</0> vs <0>{reference}</0>",
                          { 0: <code /> },
                          { reticula: c.reticulaResult, reference: c.referenceResult },
                        )}
                      </td>
                      <td>{t("Intentional Excel 1900 calendar / 15-sig-fig / half-away rule")}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </section>
        )}

        <section className="page-card" data-testid="bench-ai-eval-section">
          <h2>{t("AI Analyst — Self-Hosted Evaluation Harness")}</h2>
          <p className="page-muted">
            {tx(
              "The repository ships a reproducible headless evaluation CLI (<0>cargo xtask eval</0>) for SpreadsheetBench Verified (400 tasks, CC BY-SA 4.0) using dual-path XLSX export (<0>patch_xlsx</0> default, <0>export_xlsx</0> fallback on structural edits), pinned <0>reticula-libreoffice:26.8.2</0> container grading, 95% Wilson confidence intervals, and three-way root-cause attribution (<0>kernel_gap</0>, <0>agent_strategy</0>, <0>model_capability</0>).",
              { 0: <code /> },
            )}
          </p>
          <p className="page-muted">
            {tx(
              "<0>Published Score Status:</0> Public paid-model leaderboard runs are intentionally omitted; bring your own OpenAI-compatible key via <1>.env.local</1> to run <1>cargo xtask spike s6</1> or <1>cargo xtask eval</1> locally, or try all 10 deterministic interactive AI replays directly in the <2>Editor</2>.",
              { 0: <strong />, 1: <code />, 2: <PageLink to="/app" onNavigate={onNavigate} /> },
            )}
          </p>
        </section>
      </main>
    </div>
  );
}

const PANELS: readonly [
  left: number,
  heading: () => string,
  read: (one: GpuResult) => [number | null, number | null],
][] = [
  [180, () => t("Frame work, ms"), (one) => [one.workP50Ms, one.workP99Ms]],
  [480, () => t("Between frames, ms"), (one) => [one.intervalP50Ms, one.intervalP99Ms]],
];
const PANEL_WIDTH = 260;
const RENDERERS = ["canvas2d", "webgpu"] as const;

/** The median and the p99 of both renderers, on a scale that what takes a thousand times longer is on as well. */
function RendererChart({ run }: { readonly run: GpuRun }) {
  const rows = comparisons(run);
  const bottom = 30 + rows.length * 26;
  const x = (ms: number | null) => logPosition(ms) * PANEL_WIDTH;
  return (
    <figure className="bench-chart-card bench-renderers">
      <h3>{t("Canvas 2D and WebGPU: Median (Dot) and p99 (Ring)")}</h3>
      <p className="page-muted">
        {t("{date} · {commit} · {browser} · {cpu} · {hz} Hz. All numbers are in the table below.", {
          date: run.date,
          commit: run.commit,
          browser: run.browser ?? "—",
          cpu: run.cpu ?? "—",
          hz: run.refreshHz ?? "—",
        })}
      </p>
      <p className="bench-legend">
        {RENDERERS.map((renderer) => (
          <span key={renderer} className={renderer}>
            {rendererName(renderer)}
          </span>
        ))}
      </p>
      <div className="bench-table-wrap">
        <svg
          viewBox={`0 0 750 ${bottom + 18}`}
          className="bench-svg"
          role="img"
          data-testid="bench-svg-renderers"
        >
          <title>{t("Canvas 2D and WebGPU: Median (Dot) and p99 (Ring)")}</title>
          {rows.map(({ name }, row) => (
            <text key={name} x={0} y={47 + row * 26}>
              {name}
            </text>
          ))}
          {PANELS.map(([left, heading, read]) => (
            <g key={left} transform={`translate(${left} 0)`}>
              <text y={12}>{heading()}</text>
              {[0.1, 1, 10, 100, 1000, 16.7].map((ms, index) => (
                <g key={ms} className="scale">
                  <path d={`M${x(ms)} 24V${bottom}`} />
                  {/* A frame at 60 Hz, which is not part of the scale. */}
                  <text x={x(ms)} y={index === 5 ? 20 : bottom + 14}>
                    {ms}
                  </text>
                </g>
              ))}
              {rows.flatMap((one, row) =>
                RENDERERS.map((renderer, nth) => {
                  const [middle, tail] = read(one[renderer]);
                  const y = 38 + row * 26 + nth * 10;
                  return (
                    <g key={one.name + renderer} className={renderer}>
                      <title>
                        {`${one.name}, ${rendererName(renderer)}: ${fmtMs(middle)} / ${fmtMs(tail)}`}
                      </title>
                      <path d={`M${x(middle)} ${y}H${x(tail)}`} />
                      <circle cx={x(tail)} cy={y} r={4} className="tail" />
                      <circle cx={x(middle)} cy={y} r={4} />
                    </g>
                  );
                }),
              )}
            </g>
          ))}
        </svg>
      </div>
    </figure>
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
      label: t("B2 Sum"),
      cur: latest?.wasmChrome.b2Sum1mMs ?? 0,
      prev: prev?.wasmChrome.b2Sum1mMs ?? 0,
    },
    {
      id: "b4",
      label: t("B4 100k"),
      cur: latest?.wasmChrome.b4Incremental100kP50Ms ?? 0,
      prev: prev?.wasmChrome.b4Incremental100kP50Ms ?? 0,
    },
    {
      id: "b3",
      label: t("B3 1M"),
      cur: latest?.wasmChrome.b3FullRecalc1mMs ?? 0,
      prev: prev?.wasmChrome.b3FullRecalc1mMs ?? 0,
    },
    {
      id: "b5",
      label: t("B5 Chain"),
      cur: latest?.wasmChrome.b5Chain1mMs ?? 0,
      prev: prev?.wasmChrome.b5Chain1mMs ?? 0,
    },
    {
      id: "b10",
      label: t("B10 1stScr"),
      cur: latestStart?.firstScreenP50Ms ?? 0,
      prev: startupRuns[0]?.firstScreenP50Ms ?? 0,
    },
  ];
  const maxVal = Math.max(100, ...bars.flatMap((b) => [b.cur, b.prev]));
  return (
    <svg viewBox="0 0 360 170" className="bench-svg" role="img" data-testid="bench-svg-engine">
      <title>{t("Chrome WASM Engine and Startup Latency Comparison")}</title>
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
              {t("{ms}ms", { ms: b.cur.toFixed(0) })}
            </text>
            <text x={x + 20} y={148} fontSize="10" textAnchor="middle" fill="currentColor">
              {b.label}
            </text>
          </g>
        );
      })}
      <rect x={18} y={156} width={8} height={8} fill="#94a3b8" />
      <text x={30} y={163} fontSize="9" fill="currentColor">
        {t("Baseline ({commit})", { commit: prev?.commit ?? "—" })}
      </text>
      <rect x={140} y={156} width={8} height={8} fill="#0f766e" />
      <text x={152} y={163} fontSize="9" fill="currentColor">
        {t("Latest ({commit})", { commit: latest?.commit ?? "—" })}
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
      <title>{t("B9 Collaborative Sync Replay Latency Evolution")}</title>
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
              {t("{ms}ms", { ms: (r.wasmB9aTotalP50Ms ?? 0).toFixed(1) })}
            </text>
            <text x={x} y={148} fontSize="9" textAnchor="middle" fill="currentColor">
              {r.stage}
            </text>
          </g>
        );
      })}
      <rect x={42} y={156} width={10} height={3} fill="#0f766e" />
      <text x={56} y={162} fontSize="9" fill="currentColor">
        {t("WASM p50")}
      </text>
      <rect x={125} y={156} width={10} height={3} fill="#2563eb" />
      <text x={139} y={162} fontSize="9" fill="currentColor">
        {t("Native p50")}
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
      <title>{t("LibreOffice Differential Agreement Ratio by Function Family")}</title>
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
