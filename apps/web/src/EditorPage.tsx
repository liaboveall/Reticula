import { CellEditor, Grid, type GridStats, type Selection } from "@reticula/grid";
import type { EngineInfo, SheetInfo } from "@reticula/protocol";
import { useEffect, useRef, useState } from "react";
import { type EngineClient, startEngine } from "./engine/client.ts";
import { BENCHMARKS } from "./generated/benchmarks.ts";
import { hooksRequested, installHooks } from "./hooks.ts";

type EngineState =
  | { status: "starting" }
  | { status: "ready"; info: EngineInfo; sheet: SheetInfo }
  | { status: "failed"; message: string };

interface InspectedState {
  readonly rowId: string;
  readonly colId: string;
  readonly address: string;
  readonly raw: string;
  readonly display: string;
  readonly isFormula: boolean;
  readonly error: string | null;
}

/** How often the status bar refreshes frame-work statistics. */
const STATUS_INTERVAL_MS = 500;

const MARKS = {
  shell: "reticula:shell",
  engineReady: "reticula:engine-ready",
  firstScreen: "reticula:first-screen",
} as const;

const integer = new Intl.NumberFormat("en-US");
const milliseconds = new Intl.NumberFormat("en-US", {
  minimumFractionDigits: 1,
  maximumFractionDigits: 1,
});

export interface EditorPageProps {
  readonly onNavigateHome: () => void;
}

export function EditorPage({ onNavigateHome }: EditorPageProps) {
  const [isNarrow, setIsNarrow] = useState<boolean>(() =>
    typeof window !== "undefined" ? window.matchMedia("(max-width: 767px)").matches : false,
  );
  const [forceEditor, setForceEditor] = useState(false);

  useEffect(() => {
    const mq = window.matchMedia("(max-width: 767px)");
    const listener = (e: MediaQueryListEvent) => setIsNarrow(e.matches);
    mq.addEventListener("change", listener);
    return () => mq.removeEventListener("change", listener);
  }, []);

  if (isNarrow && !forceEditor && !hooksRequested()) {
    return (
      <div className="mobile-fallback" data-testid="mobile-fallback">
        <header className="mobile-header">
          <a
            href="/"
            className="mobile-brand"
            onClick={(e) => {
              e.preventDefault();
              onNavigateHome();
            }}
          >
            ← Reticula
          </a>
        </header>
        <main className="mobile-body">
          <h1>Desktop Canvas Editor</h1>
          <p className="mobile-notice">
            Reticula&apos;s desktop canvas editor requires a keyboard and pointer — please open this
            link on desktop.
          </p>

          <svg
            viewBox="0 0 360 176"
            className="mobile-preview"
            role="img"
            aria-label="Preview of Reticula 1,000,000-row spreadsheet and live formula recalculation"
          >
            <rect x="4" y="4" width="352" height="168" rx="8" className="preview-frame" />
            <rect x="4" y="4" width="352" height="28" rx="8" className="preview-bar" />
            <text x="16" y="22" className="preview-title">
              Reticula · 1,000,000 rows × 22 cols · fx =SUM(M2:M100001)
            </text>
            <line x1="4" y1="32" x2="356" y2="32" className="preview-line" />
            <text x="16" y="54" className="preview-cell">
              U1 Promo Rate
            </text>
            <rect x="164" y="39" width="74" height="22" rx="3" className="preview-focus" />
            <text x="228" y="54" textAnchor="end" className="preview-num">
              5.00%
            </text>
            <text x="16" y="80" className="preview-cell">
              U2 100k Net Total
            </text>
            <text x="228" y="80" textAnchor="end" className="preview-num">
              298,412,904.18
            </text>
            <line x1="12" y1="94" x2="348" y2="94" className="preview-line" />
            <text x="16" y="116" className="preview-muted">
              M2..M100001: =ROUND(J2*K2*(1-L2-$V$1), 2)
            </text>
            <text x="16" y="152" className="preview-status">
              recalculated 100,001 cells in {BENCHMARKS.b4Incremental100kP50Ms} ms
            </text>
          </svg>

          <div className="mobile-metrics">
            <div>
              <strong>{BENCHMARKS.b4Incremental100kP50Ms} ms</strong>
              <span>100k-formula incremental recalc (p50)</span>
            </div>
            <div>
              <strong>{BENCHMARKS.b7aInsertTopMeanUs} µs</strong>
              <span>Top-row insert on 1M-row sheet</span>
            </div>
            <div>
              <strong>{BENCHMARKS.b2Sum1mP50Ms} ms</strong>
              <span>1,000,000-row SUM scan (p50)</span>
            </div>
          </div>

          <button
            type="button"
            className="mobile-anyway"
            data-testid="open-anyway"
            onClick={() => setForceEditor(true)}
          >
            Open editor anyway
          </button>
        </main>
      </div>
    );
  }

  return <DesktopSpreadsheet onNavigateHome={onNavigateHome} />;
}

function DesktopSpreadsheet({ onNavigateHome }: EditorPageProps) {
  const hostRef = useRef<HTMLDivElement>(null);
  const gridRef = useRef<Grid | null>(null);
  const editorRef = useRef<CellEditor | null>(null);
  const clientRef = useRef<EngineClient | null>(null);
  const inspectedRef = useRef<InspectedState | null>(null);
  const pendingRowActionRef = useRef<"inserted" | "deleted">("inserted");

  const [engine, setEngine] = useState<EngineState>({ status: "starting" });
  const [stats, setStats] = useState<GridStats | null>(null);
  const [selection, setSelection] = useState<Selection | null>(null);
  const [inspected, setInspected] = useState<InspectedState | null>(null);
  const [editingText, setEditingText] = useState<string | null>(null);
  const [recalcMessage, setRecalcMessage] = useState<string | null>(null);
  const [rejectedReason, setRejectedReason] = useState<string | null>(null);
  const [showTip, setShowTip] = useState(true);

  useEffect(() => {
    const element = hostRef.current;
    if (!element) {
      return;
    }
    const selfChecks: ((digest: string) => void)[] = [];
    let lastInspectKey = "";

    const requestInspect = (sel: Selection | null, force = false) => {
      if (!sel || sel.rowId === null || sel.colId === null) {
        lastInspectKey = "";
        inspectedRef.current = null;
        setInspected(null);
        return;
      }
      const key = `${sel.rowId}:${sel.colId}`;
      if (!force && key === lastInspectKey) {
        return;
      }
      lastInspectKey = key;
      client.send({
        kind: "inspectCell",
        sheet: 0,
        rowId: sel.rowId,
        colId: sel.colId,
      });
    };

    const client = startEngine({
      onMessage(message) {
        switch (message.kind) {
          case "ready":
            performance.mark(MARKS.engineReady);
            grid.setSheet(message.sheet);
            setEngine({ status: "ready", info: message.info, sheet: message.sheet });
            break;
          case "sheetChanged": {
            grid.updateSheet(message.sheet);
            setEngine((prev) =>
              prev.status === "ready" ? { ...prev, sheet: message.sheet } : prev,
            );
            const verb = pendingRowActionRef.current;
            const extra =
              message.recalculated > 0
                ? `, recalculated ${integer.format(message.recalculated)} cells`
                : "";
            setRecalcMessage(`${verb} rows in ${milliseconds.format(message.actionMs)} ms${extra}`);
            requestInspect(grid.selectionDetails(), true);
            break;
          }
          case "cellInspected": {
            const info: InspectedState = {
              rowId: message.rowId,
              colId: message.colId,
              address: message.address,
              raw: message.raw,
              display: message.display,
              isFormula: message.isFormula,
              error: message.error ?? null,
            };
            inspectedRef.current = info;
            setInspected(info);
            break;
          }
          case "recalcProgress":
            setRecalcMessage(
              `recalculating ${integer.format(message.done)} / ${integer.format(message.total)}…`,
            );
            break;
          case "recalcDone":
            setRejectedReason(null);
            if (message.recalculated > 0) {
              setRecalcMessage(
                `recalculated ${integer.format(message.recalculated)} cells in ${milliseconds.format(message.elapsedMs)} ms`,
              );
            } else {
              setRecalcMessage(`updated cell in ${milliseconds.format(message.elapsedMs)} ms`);
            }
            requestInspect(grid.selectionDetails(), true);
            break;
          case "rejected":
            setRejectedReason(message.reason);
            break;
          case "selfChecked":
            for (const resolve of selfChecks.splice(0)) {
              resolve(message.digest);
            }
            break;
          case "failed":
            setEngine({ status: "failed", message: message.message });
            break;
        }
      },
      onPage: (page) => grid.acceptPage(page),
    });
    clientRef.current = client;

    const grid = new Grid(element, {
      subscribe: (subscription) => client.send({ kind: "subscribe", sheet: 0, ...subscription }),
      onSelectionChange: (sel) => {
        setSelection(sel);
        requestInspect(sel);
      },
      onActivateEdit: () => {
        editor.startEdit("doubleClick");
      },
    });
    gridRef.current = grid;

    const editor = new CellEditor(grid, element, {
      textOf: (cell) => {
        const cur = grid.selectionDetails();
        if (
          cur &&
          cur.row === cell.row &&
          cur.col === cell.col &&
          inspectedRef.current &&
          inspectedRef.current.rowId === cur.rowId &&
          inspectedRef.current.colId === cur.colId
        ) {
          return inspectedRef.current.raw;
        }
        return grid.textAt(cell.row, cell.col) ?? "";
      },
      onCommit: (cell, raw) => {
        const rowId = grid.rowIdAt(cell.row);
        const sel = grid.selectionDetails();
        const colId =
          sel && sel.col === cell.col && sel.colId !== null ? sel.colId : String(cell.col);
        if (rowId !== null) {
          client.send({
            kind: "editCell",
            sheet: 0,
            rowId,
            colId,
            raw,
          });
        }
      },
      onClear: () => {
        const runs = grid.selectedCellRuns();
        if (runs) {
          client.send({
            kind: "clearCells",
            sheet: 0,
            rows: runs.rows,
            cols: runs.cols,
          });
        }
      },
      onTextChange: (text, editingCell) => {
        setEditingText(editingCell ? text : null);
      },
    });
    editorRef.current = editor;

    let shellMarked = false;
    const stopMarking = grid.watchFrames((frame) => {
      if (!shellMarked) {
        shellMarked = true;
        performance.mark(MARKS.shell);
      }
      if (frame.cells > 0 && frame.missing === 0) {
        performance.mark(MARKS.firstScreen);
        stopMarking();
      }
    });

    const removeHooks = hooksRequested()
      ? installHooks(grid, () => {
          client.send({ kind: "selfCheck" });
          return new Promise<string>((resolve) => selfChecks.push(resolve));
        })
      : undefined;
    const timer = window.setInterval(() => setStats(grid.stats()), STATUS_INTERVAL_MS);

    return () => {
      window.clearInterval(timer);
      stopMarking();
      removeHooks?.();
      editor.destroy();
      grid.destroy();
      client.stop();
      editorRef.current = null;
      gridRef.current = null;
      clientRef.current = null;
    };
  }, []);

  const formulaBarValue =
    editingText !== null ? editingText : (inspected?.raw ?? selection?.text ?? "");

  const handleInsertRowAbove = () => {
    const grid = gridRef.current;
    const client = clientRef.current;
    if (!grid || !client) {
      return;
    }
    const targetRow = selection ? selection.range.firstRow : 0;
    const afterRowId = targetRow === 0 ? null : grid.rowIdAt(targetRow - 1);
    if (targetRow > 0 && afterRowId === null) {
      return;
    }
    pendingRowActionRef.current = "inserted";
    client.send({
      kind: "insertRows",
      sheet: 0,
      afterRowId,
      count: 1,
    });
    editorRef.current?.focus();
  };

  const handleDeleteSelectedRows = () => {
    const grid = gridRef.current;
    const client = clientRef.current;
    if (!grid || !client) {
      return;
    }
    const rows = grid.selectedRowRuns();
    if (!rows || rows.length === 0) {
      return;
    }
    pendingRowActionRef.current = "deleted";
    client.send({
      kind: "deleteRows",
      sheet: 0,
      rows,
    });
    editorRef.current?.focus();
  };

  const handleJumpToV1 = () => {
    const grid = gridRef.current;
    if (!grid) {
      return;
    }
    grid.select({ row: 0, col: 21 });
    grid.reveal(0, 21);
    editorRef.current?.focus();
  };

  const canDeleteRows = selection !== null && selection.rowId !== null;

  return (
    <div className="app">
      <header className="titlebar">
        <a
          href="/"
          className="brand-link"
          onClick={(event) => {
            event.preventDefault();
            onNavigateHome();
          }}
        >
          <h1>Reticula</h1>
        </a>
        {engine.status === "ready" && (
          <span className="document" data-testid="document">
            Sample sales data · {integer.format(engine.sheet.rows)} rows × {engine.sheet.cols}{" "}
            columns · {integer.format(engine.sheet.formulaCount)} formulas
          </span>
        )}
        <span className="titlebar-spacer" />
        <div className="toolbar">
          <button
            type="button"
            className="toolbar-btn"
            data-testid="insert-row-above"
            disabled={engine.status !== "ready"}
            onClick={handleInsertRowAbove}
            title="Insert 1 row above the selected row (at row 1 if nothing is selected)"
          >
            + Insert Row Above
          </button>
          <button
            type="button"
            className="toolbar-btn"
            data-testid="delete-rows"
            disabled={engine.status !== "ready" || !canDeleteRows}
            onClick={handleDeleteSelectedRows}
            title="Delete all rows spanned by the current selection"
          >
            Delete Selected Rows
          </button>
        </div>
      </header>

      {showTip && engine.status === "ready" && (
        <div className="demo-tip" data-testid="demo-tip">
          <span>
            Try live recalculation: edit parameter cell <code>V1</code> (e.g. type <code>5%</code>{" "}
            and press Enter) to recalculate <strong>100,001</strong> dependent formulas.
          </span>
          <button
            type="button"
            className="tip-action"
            data-testid="jump-v1"
            onClick={handleJumpToV1}
          >
            Jump to V1
          </button>
          <button
            type="button"
            className="tip-close"
            aria-label="Dismiss tip"
            onClick={() => setShowTip(false)}
          >
            ×
          </button>
        </div>
      )}

      <div className="formulabar">
        <output className="name-box" data-testid="name-box" aria-label="Selected cell or range">
          {selection ? selection.name : "—"}
        </output>
        <span className="fx-badge" aria-hidden="true">
          fx
        </span>
        <input
          type="text"
          className="formula-input"
          data-testid="formula-input"
          aria-label="Formula bar"
          placeholder={selection ? "Enter a value or =formula" : "Select a cell to edit"}
          disabled={engine.status !== "ready" || selection === null}
          value={formulaBarValue}
          onChange={(event) => {
            const next = event.target.value;
            setEditingText(next);
            editorRef.current?.setTextExternal(next);
          }}
          onKeyDown={(event) => {
            if (event.key === "Enter") {
              event.preventDefault();
              setEditingText(null);
              editorRef.current?.commitExternal(true);
            } else if (event.key === "Escape") {
              event.preventDefault();
              setEditingText(null);
              editorRef.current?.cancelExternal();
            }
          }}
        />
      </div>

      {rejectedReason && (
        <div className="rejected-banner" role="alert" data-testid="rejected-banner">
          <span>{rejectedReason}</span>
          <button
            type="button"
            className="tip-close"
            aria-label="Dismiss error"
            onClick={() => setRejectedReason(null)}
          >
            ×
          </button>
        </div>
      )}

      <div className="sheet">
        <div ref={hostRef} className="grid" data-testid="grid" />
        {engine.status === "failed" && (
          <div className="failure" role="alert">
            <p>The engine has stopped. Reload the page to start again.</p>
            <p className="detail" data-testid="engine-error">
              {engine.message}
            </p>
          </div>
        )}
      </div>

      <footer className="statusbar">
        <span data-testid="engine-status">{engine.status}</span>
        {engine.status === "starting" && <span>Generating the sample…</span>}
        {engine.status === "ready" && (
          <>
            {(selection ?? stats?.selection) && (
              <span data-testid="selection">
                {(selection ?? stats?.selection)?.name}
                {(selection ?? stats?.selection)?.text
                  ? ` · ${(selection ?? stats?.selection)?.text}`
                  : ""}
              </span>
            )}
            {recalcMessage && (
              <span className="recalc-pill" data-testid="recalc-status">
                {recalcMessage}
              </span>
            )}
            <span className="spacer" />
            <span title="The sample is computed from this seed, not downloaded">
              seed {engine.sheet.seed}
            </span>
            <span data-testid="generation" title="Time the engine took to generate the sample">
              generated in {milliseconds.format(engine.sheet.generationMs)} ms
            </span>
            {stats && stats.frameWork.samples > 0 && (
              <span
                data-testid="frame-work"
                title="Main-thread work per frame: input, receiving pages and issuing draw calls"
              >
                frame {milliseconds.format(stats.frameWork.p50)} ms · p99{" "}
                {milliseconds.format(stats.frameWork.p99)} ms
              </span>
            )}
            <span data-testid="kernel-version">kernel {engine.info.kernelVersion}</span>
          </>
        )}
      </footer>
    </div>
  );
}
