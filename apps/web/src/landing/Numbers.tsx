import type { Measurement } from "../generated/benchmarks.ts";
import { BENCHMARKS, PROVENANCE, TABLES, TABLES_PROVENANCE } from "../generated/benchmarks.ts";
import { fmtInt, t } from "../i18n/index.ts";
import { measured, mib, ms, seconds, us } from "./numbers.ts";
import { Source } from "./Source.tsx";

interface Cell {
  readonly id: string;
  readonly label: string;
  readonly value: string;
  /** The conditions, and what else the same run measured. */
  readonly detail: string;
  readonly from: Measurement | "tables";
}

const int = (value: number | null) => measured(value, fmtInt);
const size = (bytes: number | null) => measured(bytes, (value) => mib(value / 2 ** 20));

// What a workload consists of (100,000 formulas, a million rows) is part of its
// definition in the benchmarks; everything that was measured or chosen for a
// run is read off its report.
function cells(): readonly Cell[] {
  const b = BENCHMARKS;
  return [
    {
      id: "startup",
      label: t("First visit to first cells"),
      value: measured(b.startupFirstScreenP50Ms, seconds),
      detail: t("{visits} visits, no cache, {mbps} Mbit/s, {rtt} ms round trip", {
        visits: int(b.startupVisits),
        mbps: int(b.startupDownMbps),
        rtt: int(b.startupRoundTripMs),
      }),
      from: "startup",
    },
    {
      id: "open",
      label: t("Open 20 million cells and a million formulas"),
      value: measured(b.b1Load20m1mP50Ms, ms),
      detail: t("from local storage to the first page of cells"),
      from: "engine",
    },
    {
      id: "recalc",
      label: t("Recalculate 100,000 formulas"),
      value: measured(b.b4Incremental100kP50Ms, ms),
      detail: t("after one edit; p95 {p95}", { p95: measured(b.b4Incremental100kP95Ms, ms) }),
      from: "engine",
    },
    {
      id: "insert",
      label: t("Insert a row above a million"),
      value: measured(b.b7aInsertTopMeanUs, us),
      detail: t("mean of 100"),
      from: "engine",
    },
    {
      id: "scroll",
      label: t("Main thread per frame of scrolling"),
      value: measured(b.renderScrollWorkP99Ms, ms),
      detail: t("p99 of {frames} frames, a million rows, Canvas 2D", {
        frames: int(b.renderScrollFrames),
      }),
      from: "render",
    },
    {
      id: "memory",
      label: t("Memory for 50 million cells"),
      value: measured(b.b12PeakWasmMiB, mib),
      detail: t("peak of WebAssembly; opened in {load}", {
        load: measured(b.b12Load50mP50Ms, ms),
      }),
      from: "engine",
    },
    {
      id: "save",
      label: t("Answers while 20 million cells are saved"),
      value: measured(b.b11ProbeP95Ms, ms),
      detail: t("p95; the save takes {save}", { save: measured(b.b11Save20m1mP50Ms, ms) }),
      from: "engine",
    },
    {
      id: "replay",
      label: t("Remote edit under 1,000 unsent ones"),
      value: measured(b.b9aFrameP50Ms, ms),
      detail: t("to the first frame, with 100,000 formulas"),
      from: "sync",
    },
    {
      id: "load",
      label: t("Edit reaching everyone else, p95"),
      value: measured(b.loadRtt50P95Ms, ms),
      detail: t("{clients} clients, {rate} edits a second, {rtt} ms round trip added", {
        clients: int(b.loadClients),
        rate: int(b.loadTxnsPerSec),
        rtt: int(b.loadAddedRoundTripMs),
      }),
      from: "load",
    },
  ];
}

/** The same of data tables, once there is a report about them. */
function tableCells(): readonly Cell[] {
  if (!TABLES) {
    return [];
  }
  const b = TABLES;
  const rows = int(b.rows);
  return [
    {
      id: "table-import",
      label: t("Import {rows} rows of CSV", { rows }),
      value: measured(b.importMs, seconds),
      detail: t("{text} in {columns} columns, into a table of {file}", {
        text: size(b.textBytes),
        columns: int(b.columns),
        file: size(b.fileBytes),
      }),
      from: "tables",
    },
    {
      id: "table-sort",
      label: t("Sort {rows} rows by a number", { rows }),
      value: measured(b.sortNumberMs, ms),
      detail: t("by a text {text}; filter by a number {filter}", {
        text: measured(b.sortTextMs, ms),
        filter: measured(b.filterNumberMs, ms),
      }),
      from: "tables",
    },
    {
      id: "table-scroll",
      label: t("Main thread per frame, {rows} rows", { rows }),
      value: measured(b.scrollWorkP99Ms, ms),
      detail: t("p99 of scrolling, the worst of both renderers; {n} frames with placeholders", {
        n: int(b.scrollPlaceholderFrames),
      }),
      from: "tables",
    },
  ];
}

export function Numbers() {
  return (
    <section id="numbers" className="landing-section" aria-labelledby="numbers-heading">
      <h2 id="numbers-heading">{t("Numbers")}</h2>
      <p className="landing-intro">
        {t(
          "Medians unless noted, each with its commit and report. Everything in a browser ran on one fast laptop.",
        )}
      </p>
      <ul className="landing-cells">
        {[...cells(), ...tableCells()].map(({ id, label, value, detail, from }) => (
          <li key={id} className="landing-cell" data-testid={`number-${id}`}>
            <h3>{label}</h3>
            <p className="landing-value">{value}</p>
            <p className="landing-detail">{detail}</p>
            <Source
              of={from === "tables" ? TABLES_PROVENANCE : PROVENANCE[from]}
              ran={from === "load" ? "natively" : "in a browser"}
            />
          </li>
        ))}
      </ul>
    </section>
  );
}
