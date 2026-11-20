import {
  CellEditor,
  DARK_PALETTE_FILL,
  DARK_PALETTE_TEXT,
  Grid,
  type GridContextTarget,
  type GridStats,
  LIGHT_PALETTE_FILL,
  LIGHT_PALETTE_TEXT,
  type Selection,
} from "@reticula/grid";
import type {
  EngineInfo,
  SheetInfo,
  SheetTabInfo,
  WireCellStyle,
  WireCompletionItem,
  WireFormatPatch,
  WireFormulaAnalysis,
  WireHorizontalAlign,
  WireStylePatch,
} from "@reticula/protocol";
import { lazy, Suspense, useCallback, useEffect, useRef, useState } from "react";
import {
  type EngineClient,
  type HostCommand,
  type SafeModeInfo,
  startEngine,
} from "./engine/client.ts";
import type { StorageBackendKind, WorkbookCatalogEntry } from "./engine/storage.ts";
import { FormulaBar, type FormulaBarHandle } from "./FormulaBar.tsx";
import { BENCHMARKS } from "./generated/benchmarks.ts";
import {
  type HookDocStatus,
  hooksRequested,
  installHooks,
  type SnapshotSaveResult,
} from "./hooks.ts";
import type { LocalModalMode } from "./LocalModals.tsx";
import { createSwClient, type SwClientHandle } from "./sw-client.ts";

const LazyLocalModals = lazy(() => import("./LocalModals.tsx"));

type EngineState =
  | { status: "starting" }
  | { status: "ready"; info: EngineInfo; sheet: SheetInfo }
  | { status: "failed"; message: string };

type LockOverlayState =
  | { readonly kind: "blocked"; readonly docId: string; readonly title: string }
  | {
      readonly kind: "takingOver";
      readonly docId: string;
      readonly title: string;
      readonly waitingForFile: boolean;
    }
  | {
      readonly kind: "yielded";
      readonly docId: string;
      readonly title: string;
      readonly evicted: boolean;
    };

interface InspectedState {
  readonly sheet: number;
  readonly rowId: string;
  readonly colId: string;
  readonly address: string;
  readonly raw: string;
  readonly display: string;
  readonly isFormula: boolean;
  readonly error: string | null;
  readonly formatCode: string;
  readonly style: WireCellStyle;
}

interface ActiveEditSession {
  hostSheet: number;
  row: number;
  col: number;
  rowId: string;
  colId: string;
  address: string;
  source: string;
  cursorUtf16: number;
  canInsertRef: boolean;
  activeRefSpan: [number, number] | null;
  pointPickedSpan: [number, number] | null;
}

interface CopiedSnapshot {
  readonly sheet: number;
  readonly firstRow: number;
  readonly rowCount: number;
  readonly firstCol: number;
  readonly colCount: number;
  readonly cut: boolean;
  readonly tsv: string;
  readonly html: string;
}

type ContextMenuState =
  | { readonly kind: "grid"; readonly target: GridContextTarget }
  | {
      readonly kind: "tab";
      readonly clientX: number;
      readonly clientY: number;
      readonly tab: SheetTabInfo;
    };

const DEFAULT_DOC_ID = "sample-1m";
const STATUS_INTERVAL_MS = 500;
const YIELD_WATCHDOG_MS = 500;

const MARKS = {
  shell: "reticula:shell",
  engineReady: "reticula:engine-ready",
  firstScreen: "reticula:first-screen",
} as const;

const NUMBER_FORMAT_OPTIONS: readonly { readonly code: string; readonly label: string }[] = [
  { code: "General", label: "General" },
  { code: "0", label: "Integer (0)" },
  { code: "0.00", label: "Number (0.00)" },
  { code: "#,##0.00", label: "Thousands (#,##0.00)" },
  { code: "$#,##0.00", label: "USD ($#,##0.00)" },
  { code: "¥#,##0.00", label: "CNY (¥#,##0.00)" },
  { code: "€#,##0.00", label: "EUR (€#,##0.00)" },
  { code: "0%", label: "Percent (0%)" },
  { code: "0.00%", label: "Percent (0.00%)" },
  { code: "0.00E+00", label: "Scientific (0.00E+00)" },
  { code: "yyyy-mm-dd", label: "Date (yyyy-mm-dd)" },
  { code: "yyyy-mm-dd hh:mm", label: "DateTime (yyyy-mm-dd hh:mm)" },
  { code: "@", label: "Plain Text (@)" },
];

const integer = new Intl.NumberFormat("en-US");
const milliseconds = new Intl.NumberFormat("en-US", {
  minimumFractionDigits: 1,
  maximumFractionDigits: 1,
});

function computeLocalExcelSerialNow(): number {
  const now = new Date();
  const localMs = Date.UTC(
    now.getFullYear(),
    now.getMonth(),
    now.getDate(),
    now.getHours(),
    now.getMinutes(),
    now.getSeconds(),
    now.getMilliseconds(),
  );
  return 25569 + localMs / 86_400_000;
}

function nextEvalSeed(): string {
  const words = new Uint32Array(2);
  crypto.getRandomValues(words);
  const hi = BigInt(words[0] ?? 0);
  const lo = BigInt(words[1] ?? 0);
  return ((hi << 32n) | lo).toString();
}

function sendEvalEnv(client: EngineClient): void {
  client.send({
    kind: "setEvalEnv",
    nowSerial: computeLocalExcelSerialNow(),
    seed: nextEvalSeed(),
    timestampMs: Date.now(),
  });
}

function readInitialUrlDoc(): { docId: string; safeRequested: boolean } {
  if (typeof window === "undefined") {
    return { docId: DEFAULT_DOC_ID, safeRequested: false };
  }
  const params = new URLSearchParams(window.location.search);
  const rawDoc = params.get("doc")?.trim();
  const safeDoc = rawDoc && /^[a-zA-Z0-9._-]+$/.test(rawDoc) ? rawDoc : DEFAULT_DOC_ID;
  return {
    docId: safeDoc,
    safeRequested: params.get("safe") === "1",
  };
}

function syncUrlForDoc(docId: string): void {
  if (typeof window === "undefined") {
    return;
  }
  const url = new URL(window.location.href);
  url.searchParams.delete("safe");
  if (docId === DEFAULT_DOC_ID) {
    if (!url.searchParams.has("doc")) {
      return;
    }
    url.searchParams.delete("doc");
  } else {
    url.searchParams.set("doc", docId);
  }
  const nextRel = `${url.pathname}${url.search}${url.hash}`;
  const curRel = `${window.location.pathname}${window.location.search}${window.location.hash}`;
  if (nextRel !== curRel) {
    window.history.replaceState({}, "", nextRel);
  }
}

function downloadArrayBuffer(filename: string, bytes: ArrayBuffer): void {
  const blob = new Blob([bytes], { type: "application/octet-stream" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.style.display = "none";
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 2_000);
}

function makeFreshDocId(): string {
  return `wb-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 6)}`;
}

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
  const initialRoute = useRef(readInitialUrlDoc());
  const tabIdRef = useRef<string>(Math.random().toString(36).slice(2, 10));

  const hostRef = useRef<HTMLDivElement>(null);
  const gridRef = useRef<Grid | null>(null);
  const editorRef = useRef<CellEditor | null>(null);
  const formulaBarRef = useRef<FormulaBarHandle | null>(null);
  const clientRef = useRef<EngineClient | null>(null);
  const startWorkerFnRef = useRef<((docId: string, initialCmd: HostCommand) => void) | null>(null);
  const swClientRef = useRef<SwClientHandle | null>(null);

  const activeDocIdRef = useRef<string>(initialRoute.current.docId);
  const activeDocTitleRef = useRef<string>(
    initialRoute.current.docId === DEFAULT_DOC_ID
      ? "Sample sales data"
      : initialRoute.current.docId,
  );
  const docLifecycleRef = useRef<HookDocStatus["engineStatus"]>("starting");
  const storageBackendRef = useRef<StorageBackendKind | null>(null);
  const activeSlotRef = useRef<"a" | "b" | null>(null);
  const walSeqRef = useRef<string>("0");
  const waitingForFileRef = useRef<boolean>(false);
  const safeModeRef = useRef<SafeModeInfo | null>(null);
  const workerTerminatedRef = useRef<boolean>(false);
  const persistRequestedRef = useRef<boolean>(false);

  const activeSheetRef = useRef<number>(0);
  const inspectedRef = useRef<InspectedState | null>(null);
  const editSessionRef = useRef<ActiveEditSession | null>(null);
  const analysisRef = useRef<WireFormulaAnalysis | null>(null);
  const lastCopiedRef = useRef<CopiedSnapshot | null>(null);
  const pendingActionVerbRef = useRef<string>("updated");

  const [engine, setEngine] = useState<EngineState>({ status: "starting" });
  const [docId, setDocId] = useState<string>(initialRoute.current.docId);
  const [docTitle, setDocTitle] = useState<string>(activeDocTitleRef.current);
  const [catalogDocs, setCatalogDocs] = useState<readonly WorkbookCatalogEntry[]>([]);
  const [lockOverlay, setLockOverlay] = useState<LockOverlayState | null>(null);
  const [modalMode, setModalMode] = useState<LocalModalMode | null>(null);
  const [quotaBanner, setQuotaBanner] = useState<string | null>(null);
  const [swUpdateReady, setSwUpdateReady] = useState(false);
  const [swUpdating, setSwUpdating] = useState(false);
  const [csvProgress, setCsvProgress] = useState<{
    readonly rowsParsed: number;
    readonly percent: number;
  } | null>(null);
  const [storageUsedMb, setStorageUsedMb] = useState<string>("0.0");

  const [stats, setStats] = useState<GridStats | null>(null);
  const [selection, setSelection] = useState<Selection | null>(null);
  const [inspected, setInspected] = useState<InspectedState | null>(null);
  const [editingText, setEditingText] = useState<string | null>(null);
  const [formulaAnalysis, setFormulaAnalysis] = useState<{
    readonly source: string;
    readonly analysis: WireFormulaAnalysis;
  } | null>(null);
  const [recalcMessage, setRecalcMessage] = useState<string | null>(null);
  const [rejectedReason, setRejectedReason] = useState<string | null>(null);
  const [showTip, setShowTip] = useState(true);
  const [isDark, setIsDark] = useState<boolean>(() =>
    typeof window !== "undefined"
      ? window.matchMedia("(prefers-color-scheme: dark)").matches
      : false,
  );
  const [colorPopover, setColorPopover] = useState<"text" | "fill" | null>(null);
  const [contextMenu, setContextMenu] = useState<ContextMenuState | null>(null);
  const [renamingSheet, setRenamingSheet] = useState<{ id: number; draft: string } | null>(null);

  const refreshStorageEstimate = useCallback(() => {
    if (typeof navigator === "undefined" || !navigator.storage?.estimate) {
      return;
    }
    void navigator.storage
      .estimate()
      .then((est) => {
        const used = est.usage ?? 0;
        setStorageUsedMb((used / (1024 * 1024)).toFixed(1));
      })
      .catch(() => {});
  }, []);

  /**
   * Requests persistent browser storage lazily AFTER the user's first edit,
   * creation, or import (`14-m1b.md` 7.3.1 — never on cold start!).
   */
  const notePersistenceMutation = useCallback(() => {
    if (!persistRequestedRef.current) {
      persistRequestedRef.current = true;
      if (typeof navigator !== "undefined" && navigator.storage?.persist) {
        void navigator.storage.persist().catch(() => {});
      }
    }
    refreshStorageEstimate();
  }, [refreshStorageEstimate]);

  useEffect(() => {
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const onChange = (e: MediaQueryListEvent) => setIsDark(e.matches);
    mq.addEventListener("change", onChange);
    return () => mq.removeEventListener("change", onChange);
  }, []);

  useEffect(() => {
    if (!contextMenu && !colorPopover) {
      return;
    }
    const dismiss = (event: MouseEvent) => {
      const target = event.target;
      if (
        target instanceof Element &&
        (target.closest(".context-menu") || target.closest(".color-popover-wrap"))
      ) {
        return;
      }
      setContextMenu(null);
      setColorPopover(null);
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setContextMenu(null);
        setColorPopover(null);
      }
    };
    window.addEventListener("mousedown", dismiss);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", dismiss);
      window.removeEventListener("keydown", onKey);
    };
  }, [contextMenu, colorPopover]);

  const syncGridFormulaRefs = useCallback(
    (grid: Grid, analysis: WireFormulaAnalysis | null, sheetId: number) => {
      if (!analysis?.isFormula || !editSessionRef.current) {
        grid.setFormulaReferences([]);
        return;
      }
      grid.setFormulaReferences(
        analysis.references
          .filter((r) => r.sheet === sheetId)
          .map((r) => ({
            colorIndex: r.colorIndex,
            firstRow: r.firstRow,
            lastRow: r.lastRow,
            firstCol: r.firstCol,
            lastCol: r.lastCol,
          })),
      );
    },
    [],
  );

  const clearEditState = useCallback((grid: Grid, editor: CellEditor) => {
    editSessionRef.current = null;
    analysisRef.current = null;
    setEditingText(null);
    setFormulaAnalysis(null);
    editor.setFormulaAnalysis(null);
    grid.setFormulaReferences([]);
  }, []);

  const ensureEditSession = useCallback(
    (grid: Grid, initialSource: string, cursorUtf16: number): ActiveEditSession | null => {
      if (editSessionRef.current) {
        return editSessionRef.current;
      }
      const sel = grid.selectionDetails();
      if (!sel) {
        return null;
      }
      const rowId = sel.rowId ?? grid.rowIdAt(sel.row);
      const colId = sel.colId ?? grid.colIdAt(sel.col) ?? String(sel.col);
      if (rowId === null) {
        return null;
      }
      const session: ActiveEditSession = {
        hostSheet: activeSheetRef.current,
        row: sel.row,
        col: sel.col,
        rowId,
        colId,
        address: sel.name,
        source: initialSource,
        cursorUtf16,
        canInsertRef: initialSource === "=",
        activeRefSpan: null,
        pointPickedSpan: null,
      };
      editSessionRef.current = session;
      return session;
    },
    [],
  );

  const pendingAnalyzeSourcesRef = useRef<string[]>([]);

  const requestAnalyze = useCallback((client: EngineClient, grid: Grid, editor: CellEditor) => {
    const session = editSessionRef.current;
    if (!session?.source.startsWith("=")) {
      analysisRef.current = null;
      setFormulaAnalysis(null);
      editor.setFormulaAnalysis(null);
      grid.setFormulaReferences([]);
      if (session) {
        session.canInsertRef = false;
        session.activeRefSpan = null;
      }
      return;
    }
    pendingAnalyzeSourcesRef.current.push(session.source);
    client.send({
      kind: "analyzeFormula",
      sheet: session.hostSheet,
      source: session.source,
      cursorUtf16: session.cursorUtf16,
    });
  }, []);

  useEffect(() => {
    const element = hostRef.current;
    if (!element) {
      return;
    }
    const selfChecks: ((digest: string) => void)[] = [];
    const compactWaiters: ((res: SnapshotSaveResult) => void)[] = [];
    const closeWaiters: (() => void)[] = [];
    let lastInspectKey = "";
    let yieldWatchdog = 0;

    const requestInspect = (sel: Selection | null, force = false) => {
      const client = clientRef.current;
      if (
        !client ||
        workerTerminatedRef.current ||
        !sel ||
        sel.rowId === null ||
        sel.colId === null
      ) {
        lastInspectKey = "";
        inspectedRef.current = null;
        setInspected(null);
        return;
      }
      const sheetId = activeSheetRef.current;
      const key = `${sheetId}:${sel.rowId}:${sel.colId}`;
      if (!force && key === lastInspectKey) {
        return;
      }
      if (key !== lastInspectKey) {
        inspectedRef.current = null;
        setInspected(null);
      }
      lastInspectKey = key;
      client.send({
        kind: "inspectCell",
        sheet: sheetId,
        rowId: sel.rowId,
        colId: sel.colId,
      });
    };

    const isPointAndClickActive = (): boolean => {
      const session = editSessionRef.current;
      if (!session?.source.startsWith("=")) {
        return false;
      }
      return (
        session.pointPickedSpan !== null || session.canInsertRef || session.activeRefSpan !== null
      );
    };

    const grid = new Grid(element, {
      subscribe: (subscription) => {
        if (!workerTerminatedRef.current) {
          clientRef.current?.send({
            kind: "subscribe",
            sheet: activeSheetRef.current,
            ...subscription,
          });
        }
      },
      onSelectionChange: (sel) => {
        setSelection(sel);
        setContextMenu(null);
        requestInspect(sel);
      },
      onActivateEdit: () => {
        editor.startEdit("doubleClick");
      },
      onResizeRows: (sizes) => {
        const client = clientRef.current;
        if (!client || workerTerminatedRef.current) {
          return;
        }
        notePersistenceMutation();
        pendingActionVerbRef.current = "resized rows";
        client.send({
          kind: "resizeRows",
          sheet: activeSheetRef.current,
          sizes: sizes.map((s) => ({ run: s.run, sizePx: s.sizePx ?? undefined })),
        });
      },
      onResizeCols: (sizes) => {
        const client = clientRef.current;
        if (!client || workerTerminatedRef.current) {
          return;
        }
        notePersistenceMutation();
        pendingActionVerbRef.current = "resized columns";
        client.send({
          kind: "resizeCols",
          sheet: activeSheetRef.current,
          sizes: sizes.map((s) => ({ run: s.run, sizePx: s.sizePx ?? undefined })),
        });
      },
      onMoveRows: (rows, afterRowId) => {
        const client = clientRef.current;
        if (!client || workerTerminatedRef.current) {
          return;
        }
        notePersistenceMutation();
        pendingActionVerbRef.current = "moved rows";
        client.send({
          kind: "moveRows",
          sheet: activeSheetRef.current,
          rows: [...rows],
          afterRowId,
        });
      },
      onMoveCols: (cols, afterColId) => {
        const client = clientRef.current;
        if (!client || workerTerminatedRef.current) {
          return;
        }
        notePersistenceMutation();
        pendingActionVerbRef.current = "moved columns";
        client.send({
          kind: "moveCols",
          sheet: activeSheetRef.current,
          cols: [...cols],
          afterColId,
        });
      },
      onFillRange: (src, dst) => {
        const client = clientRef.current;
        if (!client || workerTerminatedRef.current) {
          return;
        }
        notePersistenceMutation();
        pendingActionVerbRef.current = "filled range";
        sendEvalEnv(client);
        client.send({
          kind: "fillRange",
          sheet: activeSheetRef.current,
          srcFirstRow: src.firstRow,
          srcRowCount: src.endRow - src.firstRow,
          srcFirstCol: src.firstCol,
          srcColCount: src.endCol - src.firstCol,
          dstFirstRow: dst.firstRow,
          dstRowCount: dst.endRow - dst.firstRow,
          dstFirstCol: dst.firstCol,
          dstColCount: dst.endCol - dst.firstCol,
        });
        grid.selectRange(
          { row: dst.firstRow, col: dst.firstCol },
          { row: dst.endRow - 1, col: dst.endCol - 1 },
        );
      },
      onAutoFillDown: (src) => {
        const client = clientRef.current;
        if (!client || workerTerminatedRef.current) {
          return;
        }
        notePersistenceMutation();
        pendingActionVerbRef.current = "auto-filled column";
        sendEvalEnv(client);
        client.send({
          kind: "autoFillDown",
          sheet: activeSheetRef.current,
          srcFirstRow: src.firstRow,
          srcRowCount: src.endRow - src.firstRow,
          srcFirstCol: src.firstCol,
          srcColCount: src.endCol - src.firstCol,
        });
      },
      onPointAndClickRange: (range) => {
        const client = clientRef.current;
        if (!client || workerTerminatedRef.current || !isPointAndClickActive()) {
          return false;
        }
        const session = editSessionRef.current;
        if (!session) {
          return false;
        }
        if (session.pointPickedSpan === null) {
          session.pointPickedSpan = session.activeRefSpan ?? [
            session.cursorUtf16,
            session.cursorUtf16,
          ];
        }
        client.send({
          kind: "formatRangeRef",
          hostSheet: session.hostSheet,
          targetSheet: activeSheetRef.current,
          r1: range.firstRow,
          c1: range.firstCol,
          r2: Math.max(range.firstRow, range.endRow - 1),
          c2: Math.max(range.firstCol, range.endCol - 1),
        });
        return true;
      },
      onContextMenu: (target) => {
        setColorPopover(null);
        setContextMenu({ kind: "grid", target });
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
          inspectedRef.current.sheet === activeSheetRef.current &&
          inspectedRef.current.rowId === cur.rowId &&
          inspectedRef.current.colId === cur.colId
        ) {
          return inspectedRef.current.raw;
        }
        return grid.textAt(cell.row, cell.col) ?? "";
      },
      onCommit: (cell, raw) => {
        const client = clientRef.current;
        const session = editSessionRef.current;
        clearEditState(grid, editor);
        if (!client || workerTerminatedRef.current) {
          return;
        }
        const sheet = session ? session.hostSheet : activeSheetRef.current;
        const rowId = session ? session.rowId : grid.rowIdAt(cell.row);
        const sel = grid.selectionDetails();
        const colId = session
          ? session.colId
          : sel && sel.col === cell.col && sel.colId !== null
            ? sel.colId
            : (grid.colIdAt(cell.col) ?? String(cell.col));
        if (rowId !== null) {
          notePersistenceMutation();
          sendEvalEnv(client);
          client.send({
            kind: "editCell",
            sheet,
            rowId,
            colId,
            raw,
          });
        }
        if (session && activeSheetRef.current !== session.hostSheet) {
          client.send({ kind: "switchSheet", sheet: session.hostSheet });
        }
      },
      onClear: () => {
        const client = clientRef.current;
        const runs = grid.selectedCellRuns();
        if (client && !workerTerminatedRef.current && runs) {
          notePersistenceMutation();
          pendingActionVerbRef.current = "cleared cells";
          client.send({
            kind: "clearCells",
            sheet: activeSheetRef.current,
            rows: runs.rows,
            cols: runs.cols,
          });
        }
      },
      onTextChange: (text, editingCell, cursorUtf16) => {
        const client = clientRef.current;
        if (!editingCell) {
          if (
            editSessionRef.current &&
            activeSheetRef.current !== editSessionRef.current.hostSheet
          ) {
            return;
          }
          clearEditState(grid, editor);
          return;
        }
        const session = ensureEditSession(grid, text, cursorUtf16);
        if (session) {
          if (session.source !== text) {
            session.pointPickedSpan = null;
          }
          session.source = text;
          session.cursorUtf16 = cursorUtf16;
          session.canInsertRef = text === "=";
        }
        setEditingText(text);
        if (client && !workerTerminatedRef.current) {
          requestAnalyze(client, grid, editor);
        }
      },
      onToggleDollar: (text, cursorUtf16) => {
        clientRef.current?.send({ kind: "toggleFormulaDollar", source: text, cursorUtf16 });
      },
      onFormulaKeyDown: (event, text, cursorUtf16) => {
        return formulaBarRef.current?.handleAssistKeyDown(event, text, cursorUtf16) ?? false;
      },
      isPointAndClickActive,
      onJumpEdge: (dir, select) => {
        const cur = grid.selection;
        if (cur && !workerTerminatedRef.current) {
          clientRef.current?.send({
            kind: "jumpEdge",
            sheet: activeSheetRef.current,
            row: cur.row,
            col: cur.col,
            dir,
            select,
          });
        }
      },
      onJumpUsedEnd: (select) => {
        if (!workerTerminatedRef.current) {
          clientRef.current?.send({
            kind: "jumpUsedEnd",
            sheet: activeSheetRef.current,
            select,
          });
        }
      },
      onUndo: () => {
        if (!workerTerminatedRef.current) {
          notePersistenceMutation();
          pendingActionVerbRef.current = "undid action";
          clientRef.current?.send({ kind: "undo" });
        }
      },
      onRedo: () => {
        if (!workerTerminatedRef.current) {
          notePersistenceMutation();
          pendingActionVerbRef.current = "redid action";
          clientRef.current?.send({ kind: "redo" });
        }
      },
      onCopy: (cut, clipboardData) => {
        const range = grid.selectionRange;
        if (!range || workerTerminatedRef.current) {
          return;
        }
        if (
          clipboardData &&
          range.endRow - range.firstRow === 1 &&
          range.endCol - range.firstCol === 1
        ) {
          const single = grid.textAt(range.firstRow, range.firstCol);
          if (single !== null) {
            clipboardData.setData("text/plain", single);
          }
        }
        clientRef.current?.send({
          kind: "copyRange",
          sheet: activeSheetRef.current,
          firstRow: range.firstRow,
          rowCount: range.endRow - range.firstRow,
          firstCol: range.firstCol,
          colCount: range.endCol - range.firstCol,
          cut,
        });
      },
      onPaste: (tsv, html) => {
        const client = clientRef.current;
        const range = grid.selectionRange;
        if (!client || workerTerminatedRef.current || !range) {
          return;
        }
        let effTsv = tsv;
        let effHtml = html ?? undefined;
        if (effTsv.length === 0 && !effHtml && lastCopiedRef.current) {
          effTsv = lastCopiedRef.current.tsv;
          effHtml = lastCopiedRef.current.html;
        }
        if (effTsv.length === 0 && !effHtml) {
          return;
        }
        notePersistenceMutation();
        sendEvalEnv(client);
        client.send({
          kind: "pasteClipboard",
          sheet: activeSheetRef.current,
          destRow: range.firstRow,
          destRowCount: range.endRow - range.firstRow,
          destCol: range.firstCol,
          destColCount: range.endCol - range.firstCol,
          tsv: effTsv,
          html: effHtml,
        });
      },
    });
    editorRef.current = editor;

    const mountWorkerClient = (targetDocId: string, initialCmd: HostCommand) => {
      clientRef.current?.stop();
      workerTerminatedRef.current = false;
      activeDocIdRef.current = targetDocId;
      docLifecycleRef.current = initialCmd.kind === "takeOverDoc" ? "takingOver" : "starting";

      const client = startEngine({
        tabId: tabIdRef.current,
        initialDocId: targetDocId,
        initialCommand: initialCmd,
        onMessage(message) {
          switch (message.kind) {
            case "docOpened": {
              window.clearTimeout(yieldWatchdog);
              activeDocIdRef.current = message.docId;
              activeDocTitleRef.current = message.title;
              storageBackendRef.current = message.storageBackend;
              activeSlotRef.current = message.activeSlot;
              waitingForFileRef.current = false;
              safeModeRef.current = null;
              setDocId(message.docId);
              setDocTitle(message.title);
              setLockOverlay(null);
              setCsvProgress(null);
              setModalMode(null);
              syncUrlForDoc(message.docId);
              refreshStorageEstimate();
              break;
            }
            case "docBlocked": {
              activeDocIdRef.current = message.docId;
              activeDocTitleRef.current = message.title;
              docLifecycleRef.current = "blocked";
              waitingForFileRef.current = false;
              setDocId(message.docId);
              setDocTitle(message.title);
              setLockOverlay({ kind: "blocked", docId: message.docId, title: message.title });
              break;
            }
            case "docTakingOver": {
              docLifecycleRef.current = "takingOver";
              waitingForFileRef.current = message.waitingForFile;
              setLockOverlay({
                kind: "takingOver",
                docId: message.docId,
                title: activeDocTitleRef.current,
                waitingForFile: message.waitingForFile,
              });
              break;
            }
            case "docYielded": {
              window.clearTimeout(yieldWatchdog);
              docLifecycleRef.current = "yielded";
              waitingForFileRef.current = false;
              clearEditState(grid, editor);
              setLockOverlay({
                kind: "yielded",
                docId: message.docId,
                title: activeDocTitleRef.current,
                evicted: message.evicted,
              });
              break;
            }
            case "safeMode": {
              docLifecycleRef.current = "safeMode";
              safeModeRef.current = message.info;
              activeDocIdRef.current = message.info.docId;
              activeDocTitleRef.current = message.info.title;
              setDocId(message.info.docId);
              setDocTitle(message.info.title);
              setLockOverlay(null);
              setModalMode({ kind: "safeMode", info: message.info });
              break;
            }
            case "catalogListed": {
              setCatalogDocs(message.docs);
              const activeEntry = message.docs.find((d) => d.id === activeDocIdRef.current);
              if (activeEntry) {
                activeDocTitleRef.current = activeEntry.title;
                setDocTitle(activeEntry.title);
              }
              refreshStorageEstimate();
              break;
            }
            case "docSaved": {
              activeSlotRef.current = message.slot;
              refreshStorageEstimate();
              for (const resolve of compactWaiters.splice(0)) {
                resolve({
                  docId: message.docId,
                  slot: message.slot,
                  byteLen: message.byteLen,
                  elapsedMs: message.elapsedMs,
                });
              }
              break;
            }
            case "quotaExceeded": {
              setQuotaBanner(message.message);
              break;
            }
            case "csvImportProgress": {
              setCsvProgress({ rowsParsed: message.rowsParsed, percent: message.percent });
              break;
            }
            case "fileExported": {
              downloadArrayBuffer(message.filename, message.bytes);
              break;
            }
            case "closedForUpdate": {
              for (const resolve of closeWaiters.splice(0)) {
                resolve();
              }
              break;
            }
            case "ready":
              performance.mark(MARKS.engineReady);
              docLifecycleRef.current = "ready";
              activeSheetRef.current = message.sheet.sheet;
              walSeqRef.current = message.sheet.walSeq;
              lastInspectKey = "";
              inspectedRef.current = null;
              setInspected(null);
              grid.setSheet(message.sheet);
              setEngine({ status: "ready", info: message.info, sheet: message.sheet });
              break;
            case "sheetChanged": {
              const switched = message.sheet.sheet !== activeSheetRef.current;
              activeSheetRef.current = message.sheet.sheet;
              walSeqRef.current = message.sheet.walSeq;
              if (switched) {
                lastInspectKey = "";
                inspectedRef.current = null;
                setInspected(null);
                grid.setSheet(message.sheet);
                const cp = lastCopiedRef.current;
                if (cp && cp.sheet === message.sheet.sheet) {
                  grid.setClipboardRange(
                    {
                      firstRow: cp.firstRow,
                      endRow: cp.firstRow + cp.rowCount,
                      firstCol: cp.firstCol,
                      endCol: cp.firstCol + cp.colCount,
                    },
                    cp.cut,
                  );
                }
              } else {
                grid.updateSheet(message.sheet);
              }
              syncGridFormulaRefs(grid, analysisRef.current, message.sheet.sheet);
              setEngine((prev) =>
                prev.status === "ready" ? { ...prev, sheet: message.sheet } : prev,
              );
              const verb = pendingActionVerbRef.current;
              pendingActionVerbRef.current = "updated";
              const extra =
                message.recalculated > 0
                  ? `, recalculated ${integer.format(message.recalculated)} cells`
                  : "";
              setRecalcMessage(`${verb} in ${milliseconds.format(message.actionMs)} ms${extra}`);
              requestInspect(grid.selectionDetails(), true);
              refreshStorageEstimate();
              break;
            }
            case "stylesUpdated": {
              if (message.sheet === activeSheetRef.current) {
                grid.setStyles(message.styles);
              }
              break;
            }
            case "cellInspected": {
              const cur = grid.selectionDetails();
              if (
                !cur ||
                message.sheet !== activeSheetRef.current ||
                cur.rowId !== message.rowId ||
                cur.colId !== message.colId
              ) {
                break;
              }
              const info: InspectedState = {
                sheet: message.sheet,
                rowId: message.rowId,
                colId: message.colId,
                address: message.address,
                raw: message.raw,
                display: message.display,
                isFormula: message.isFormula,
                error: message.error ?? null,
                formatCode: message.formatCode,
                style: message.style,
              };
              inspectedRef.current = info;
              setInspected(info);
              break;
            }
            case "formulaAnalyzed": {
              const analyzedSource = pendingAnalyzeSourcesRef.current.shift();
              const session = editSessionRef.current;
              if (!session || (analyzedSource !== undefined && analyzedSource !== session.source)) {
                break;
              }
              analysisRef.current = message.analysis;
              session.canInsertRef = message.analysis.canInsertRef;
              session.activeRefSpan = message.analysis.activeRefSpan ?? null;
              setFormulaAnalysis({ source: session.source, analysis: message.analysis });
              editor.setFormulaAnalysis(message.analysis);
              syncGridFormulaRefs(grid, message.analysis, activeSheetRef.current);
              break;
            }
            case "formulaDollarToggled": {
              const session = editSessionRef.current;
              if (!session || !message.changed) {
                break;
              }
              session.source = message.source;
              session.cursorUtf16 = message.endUtf16;
              session.pointPickedSpan = null;
              analysisRef.current = null;
              setFormulaAnalysis(null);
              setEditingText(message.source);
              formulaBarRef.current?.applyTextAndSelection(
                message.source,
                message.endUtf16,
                message.endUtf16,
              );
              if (activeSheetRef.current === session.hostSheet) {
                editor.applyTextAndSelection(message.source, message.endUtf16, message.endUtf16);
              }
              requestAnalyze(client, grid, editor);
              break;
            }
            case "rangeRefFormatted": {
              const session = editSessionRef.current;
              if (!session) {
                break;
              }
              const span = session.pointPickedSpan ??
                session.activeRefSpan ?? [session.cursorUtf16, session.cursorUtf16];
              const repStart = Math.max(0, Math.min(session.source.length, span[0]));
              const repEnd = Math.max(repStart, Math.min(session.source.length, span[1]));
              const nextSource =
                session.source.slice(0, repStart) +
                message.reference +
                session.source.slice(repEnd);
              const nextEnd = repStart + message.reference.length;
              session.source = nextSource;
              session.cursorUtf16 = nextEnd;
              session.pointPickedSpan = [repStart, nextEnd];
              analysisRef.current = null;
              setFormulaAnalysis(null);
              setEditingText(nextSource);
              formulaBarRef.current?.applyTextAndSelection(nextSource, nextEnd, nextEnd);
              if (activeSheetRef.current === session.hostSheet) {
                editor.setTextExternal(nextSource, nextEnd);
              }
              requestAnalyze(client, grid, editor);
              break;
            }
            case "edgeJumped": {
              if (message.sheet === activeSheetRef.current) {
                grid.jumpTo(message.row, message.col, message.select);
              }
              break;
            }
            case "clipboardCopied": {
              const snap: CopiedSnapshot = {
                sheet: message.sheet,
                firstRow: message.firstRow,
                rowCount: message.rowCount,
                firstCol: message.firstCol,
                colCount: message.firstCol === undefined ? 0 : message.colCount,
                cut: message.cut,
                tsv: message.tsv,
                html: message.html,
              };
              lastCopiedRef.current = snap;
              if (message.sheet === activeSheetRef.current) {
                grid.setClipboardRange(
                  {
                    firstRow: message.firstRow,
                    endRow: message.firstRow + message.rowCount,
                    firstCol: message.firstCol,
                    endCol: message.firstCol + message.colCount,
                  },
                  message.cut,
                );
              }
              if (typeof navigator !== "undefined" && navigator.clipboard) {
                if (typeof ClipboardItem !== "undefined" && navigator.clipboard.write) {
                  const item = new ClipboardItem({
                    "text/plain": new Blob([message.tsv], { type: "text/plain" }),
                    "text/html": new Blob([message.html], { type: "text/html" }),
                  });
                  void navigator.clipboard.write([item]).catch(() => {
                    void navigator.clipboard.writeText?.(message.tsv).catch(() => {});
                  });
                } else {
                  void navigator.clipboard.writeText?.(message.tsv).catch(() => {});
                }
              }
              break;
            }
            case "clipboardPasted": {
              activeSheetRef.current = message.sheet.sheet;
              walSeqRef.current = message.sheet.walSeq;
              grid.updateSheet(message.sheet);
              setEngine((prev) =>
                prev.status === "ready" ? { ...prev, sheet: message.sheet } : prev,
              );
              if (lastCopiedRef.current?.cut) {
                lastCopiedRef.current = null;
                grid.setClipboardRange(null);
              }
              if (message.destRowCount > 0 && message.destColCount > 0) {
                grid.selectRange(
                  { row: message.destFirstRow, col: message.destFirstCol },
                  {
                    row: message.destFirstRow + message.destRowCount - 1,
                    col: message.destFirstCol + message.destColCount - 1,
                  },
                );
              }
              const extra =
                message.recalculated > 0
                  ? `, recalculated ${integer.format(message.recalculated)} cells`
                  : "";
              setRecalcMessage(`pasted in ${milliseconds.format(message.elapsedMs)} ms${extra}`);
              requestInspect(grid.selectionDetails(), true);
              refreshStorageEstimate();
              break;
            }
            case "recalcProgress":
              setRecalcMessage(
                `recalculating ${integer.format(message.done)} / ${integer.format(message.total)}…`,
              );
              break;
            case "recalcDone":
              setRejectedReason(null);
              walSeqRef.current = message.sheet.walSeq;
              grid.updateSheet(message.sheet);
              setEngine((prev) =>
                prev.status === "ready" ? { ...prev, sheet: message.sheet } : prev,
              );
              if (message.recalculated > 0) {
                setRecalcMessage(
                  `recalculated ${integer.format(message.recalculated)} cells in ${milliseconds.format(message.elapsedMs)} ms`,
                );
              } else {
                setRecalcMessage(`updated cell in ${milliseconds.format(message.elapsedMs)} ms`);
              }
              requestInspect(grid.selectionDetails(), true);
              refreshStorageEstimate();
              break;
            case "rejected":
              setCsvProgress(null);
              setRejectedReason(message.reason);
              break;
            case "selfChecked":
              for (const resolve of selfChecks.splice(0)) {
                resolve(message.digest);
              }
              break;
            case "failed":
              docLifecycleRef.current = "failed";
              setEngine({ status: "failed", message: message.message });
              break;
          }
        },
        onPage: (page) => grid.acceptPage(page),
      });

      clientRef.current = client;
    };

    startWorkerFnRef.current = mountWorkerClient;
    mountWorkerClient(initialRoute.current.docId, {
      kind: "openDoc",
      docId: initialRoute.current.docId,
      safeRequested: initialRoute.current.safeRequested,
    });
    refreshStorageEstimate();

    // Main-thread 500 ms takeover watchdog (`14-m1b.md` 7.2 Step 3): terminates a
    // stuck Worker that fails to yield within 500 ms so its lock & OPFS file release!
    const locksWatch =
      typeof BroadcastChannel !== "undefined" ? new BroadcastChannel("reticula:locks") : null;
    if (locksWatch) {
      locksWatch.onmessage = (
        event: MessageEvent<{ kind?: string; docId?: string; fromTabId?: string }>,
      ) => {
        const data = event.data;
        if (
          data?.kind === "yield" &&
          data.fromTabId !== tabIdRef.current &&
          data.docId === activeDocIdRef.current &&
          docLifecycleRef.current === "ready"
        ) {
          window.clearTimeout(yieldWatchdog);
          yieldWatchdog = window.setTimeout(() => {
            if (docLifecycleRef.current !== "ready" || activeDocIdRef.current !== data.docId) {
              return;
            }
            clientRef.current?.stop();
            workerTerminatedRef.current = true;
            docLifecycleRef.current = "yielded";
            clearEditState(grid, editor);
            setLockOverlay({
              kind: "yielded",
              docId: activeDocIdRef.current,
              title: activeDocTitleRef.current,
              evicted: true,
            });
          }, YIELD_WATCHDOG_MS);
        }
      };
    }

    const flushAndCloseDoc = (): Promise<void> => {
      if (workerTerminatedRef.current || !clientRef.current) {
        return Promise.resolve();
      }
      clientRef.current.sendHost({ kind: "flushAndCloseForUpdate" });
      return new Promise<void>((resolve) => {
        const cap = window.setTimeout(resolve, 1_500);
        closeWaiters.push(() => {
          window.clearTimeout(cap);
          resolve();
        });
      });
    };

    let swStartTimer = 0;
    const ensureSwClient = (): SwClientHandle => {
      if (!swClientRef.current) {
        swClientRef.current = createSwClient({
          tabId: tabIdRef.current,
          onUpdateReady: (ready) => setSwUpdateReady(ready),
          onDrainForUpdate: flushAndCloseDoc,
        });
      }
      return swClientRef.current;
    };

    let shellMarked = false;
    let firstScreenMarked = false;
    const stopMarking = grid.watchFrames((frame) => {
      if (!shellMarked) {
        shellMarked = true;
        performance.mark(MARKS.shell);
      }
      if (!firstScreenMarked && frame.cells > 0 && frame.missing === 0) {
        firstScreenMarked = true;
        performance.mark(MARKS.firstScreen);
      }
      if (frame.missing === 0 && (frame.cells > 0 || grid.stats().pagesHeld > 0)) {
        if (!swClientRef.current && swStartTimer === 0) {
          // Defer Service Worker registration until after first-screen paint so
          // cold-start B10 latency is completely unaffected (`14-m1b.md` 7.4).
          swStartTimer = window.setTimeout(() => {
            swStartTimer = 0;
            ensureSwClient();
          }, 0);
        }
        if (firstScreenMarked) {
          stopMarking();
        }
      }
    });

    const removeHooks = hooksRequested()
      ? installHooks(grid, {
          selfCheck: () => {
            clientRef.current?.send({ kind: "selfCheck" });
            return new Promise<string>((resolve) => selfChecks.push(resolve));
          },
          docStatus: () => ({
            docId: activeDocIdRef.current,
            title: activeDocTitleRef.current,
            engineStatus: docLifecycleRef.current,
            storageBackend: storageBackendRef.current,
            activeSlot: activeSlotRef.current,
            walSeq: walSeqRef.current,
            waitingForFile: waitingForFileRef.current,
            safeMode: safeModeRef.current,
          }),
          compactSnapshot: () => {
            clientRef.current?.sendHost({ kind: "forceCompactSnapshot" });
            return new Promise<SnapshotSaveResult>((resolve) => compactWaiters.push(resolve));
          },
          flushAndClose: flushAndCloseDoc,
          spinWorker: (ms) => {
            clientRef.current?.sendHost({ kind: "debugSpinWorker", ms });
          },
          swStatus: () => ensureSwClient().status(),
          checkSwUpdate: () => ensureSwClient().checkForUpdate(),
        })
      : undefined;
    const timer = window.setInterval(() => setStats(grid.stats()), STATUS_INTERVAL_MS);

    // Release the dedicated worker (and thus its Web Lock and OPFS
    // SyncAccessHandle) on navigation or BFCache freeze, and transparently
    // reopen if restored from BFCache.
    const onPageHide = () => {
      window.clearTimeout(yieldWatchdog);
      clientRef.current?.stop();
      workerTerminatedRef.current = true;
    };
    const onPageShow = (event: PageTransitionEvent) => {
      if (event.persisted && workerTerminatedRef.current) {
        mountWorkerClient(activeDocIdRef.current, {
          kind: "openDoc",
          docId: activeDocIdRef.current,
        });
      }
    };
    window.addEventListener("pagehide", onPageHide);
    window.addEventListener("pageshow", onPageShow);

    return () => {
      window.removeEventListener("pagehide", onPageHide);
      window.removeEventListener("pageshow", onPageShow);
      window.clearInterval(timer);
      window.clearTimeout(yieldWatchdog);
      window.clearTimeout(swStartTimer);
      locksWatch?.close();
      swClientRef.current?.dispose();
      swClientRef.current = null;
      stopMarking();
      removeHooks?.();
      editor.destroy();
      grid.destroy();
      clientRef.current?.stop();
      editorRef.current = null;
      gridRef.current = null;
      clientRef.current = null;
      startWorkerFnRef.current = null;
    };
  }, [
    clearEditState,
    ensureEditSession,
    notePersistenceMutation,
    refreshStorageEstimate,
    requestAnalyze,
    syncGridFormulaRefs,
  ]);

  const activeSheetId = engine.status === "ready" ? engine.sheet.sheet : 0;
  const matchingInspected =
    inspected !== null &&
    selection !== null &&
    inspected.sheet === activeSheetId &&
    inspected.rowId === selection.rowId &&
    inspected.colId === selection.colId
      ? inspected
      : null;

  const formulaBarValue =
    editingText !== null ? editingText : (matchingInspected?.raw ?? selection?.text ?? "");

  const getEditSource = useCallback(() => editSessionRef.current?.source ?? null, []);

  const nameBoxText = editSessionRef.current
    ? editSessionRef.current.hostSheet !== activeSheetId
      ? `${engine.status === "ready" ? (engine.sheet.sheets.find((s) => s.id === editSessionRef.current?.hostSheet)?.name ?? "Sheet") : "Sheet"}!${editSessionRef.current.address}`
      : editSessionRef.current.address
    : selection
      ? selection.name
      : "—";

  const handleFormulaBarChange = (next: string, cursorUtf16: number) => {
    const grid = gridRef.current;
    const editor = editorRef.current;
    const client = clientRef.current;
    if (!grid || !editor || !client || workerTerminatedRef.current) {
      return;
    }
    const session = ensureEditSession(grid, next, cursorUtf16);
    if (!session) {
      return;
    }
    if (session.source !== next) {
      session.pointPickedSpan = null;
    }
    session.source = next;
    session.cursorUtf16 = cursorUtf16;
    if (next === "=") {
      session.canInsertRef = true;
    }
    setEditingText(next);
    if (activeSheetRef.current === session.hostSheet) {
      editor.setTextExternal(next, cursorUtf16);
    }
    requestAnalyze(client, grid, editor);
  };

  const handleFormulaBarCommit = (moveDown: boolean) => {
    const grid = gridRef.current;
    const editor = editorRef.current;
    const client = clientRef.current;
    if (!grid || !editor || !client || workerTerminatedRef.current) {
      return;
    }
    const session = editSessionRef.current;
    if (session && activeSheetRef.current !== session.hostSheet) {
      const { hostSheet, rowId, colId, source } = session;
      clearEditState(grid, editor);
      notePersistenceMutation();
      sendEvalEnv(client);
      client.send({ kind: "editCell", sheet: hostSheet, rowId, colId, raw: source });
      client.send({ kind: "switchSheet", sheet: hostSheet });
      editor.focus();
      return;
    }
    setEditingText(null);
    editor.commitExternal(moveDown);
  };

  const handleFormulaBarCancel = () => {
    const grid = gridRef.current;
    const editor = editorRef.current;
    const client = clientRef.current;
    if (!grid || !editor || !client) {
      return;
    }
    const session = editSessionRef.current;
    clearEditState(grid, editor);
    if (!workerTerminatedRef.current && session && activeSheetRef.current !== session.hostSheet) {
      client.send({ kind: "switchSheet", sheet: session.hostSheet });
    }
    editor.cancelExternal();
  };

  const handleAcceptCompletion = (
    currentSource: string,
    item: WireCompletionItem,
    fromFormulaBar: boolean,
  ) => {
    const grid = gridRef.current;
    const editor = editorRef.current;
    const client = clientRef.current;
    if (!grid || !editor || !client || workerTerminatedRef.current) {
      return;
    }
    const start = Math.max(0, Math.min(currentSource.length, item.replaceStartUtf16));
    const end = Math.max(start, Math.min(currentSource.length, item.replaceEndUtf16));
    const nextSource = currentSource.slice(0, start) + item.insertText + currentSource.slice(end);
    const nextCursor = start + item.insertText.length;

    const session = ensureEditSession(grid, nextSource, nextCursor);
    if (session) {
      session.source = nextSource;
      session.cursorUtf16 = nextCursor;
      session.pointPickedSpan = null;
    }
    setEditingText(nextSource);
    formulaBarRef.current?.applyTextAndSelection(nextSource, nextCursor, nextCursor);
    if (!session || activeSheetRef.current === session.hostSheet) {
      editor.applyTextAndSelection(nextSource, nextCursor, nextCursor);
    }
    if (fromFormulaBar) {
      formulaBarRef.current?.focus();
    } else {
      editor.focus();
    }
    requestAnalyze(client, grid, editor);
  };

  const handleInsertRow = (below: boolean) => {
    const grid = gridRef.current;
    const client = clientRef.current;
    if (!grid || !client || workerTerminatedRef.current) {
      return;
    }
    const baseRow = selection ? (below ? selection.range.endRow - 1 : selection.range.firstRow) : 0;
    const afterPos = below ? baseRow : baseRow - 1;
    const afterRowId = afterPos < 0 ? null : grid.rowIdAt(afterPos);
    if (afterPos >= 0 && afterRowId === null) {
      return;
    }
    notePersistenceMutation();
    pendingActionVerbRef.current = "inserted rows";
    client.send({
      kind: "insertRows",
      sheet: activeSheetRef.current,
      afterRowId,
      count: 1,
    });
    editorRef.current?.focus();
  };

  const handleDeleteSelectedRows = () => {
    const grid = gridRef.current;
    const client = clientRef.current;
    if (!grid || !client || workerTerminatedRef.current) {
      return;
    }
    const rows = grid.selectedRowRuns();
    if (!rows || rows.length === 0) {
      return;
    }
    notePersistenceMutation();
    pendingActionVerbRef.current = "deleted rows";
    client.send({
      kind: "deleteRows",
      sheet: activeSheetRef.current,
      rows,
    });
    editorRef.current?.focus();
  };

  const handleInsertCol = (right: boolean) => {
    const grid = gridRef.current;
    const client = clientRef.current;
    if (!grid || !client || workerTerminatedRef.current) {
      return;
    }
    const baseCol = selection ? (right ? selection.range.endCol - 1 : selection.range.firstCol) : 0;
    const afterPos = right ? baseCol : baseCol - 1;
    const afterColId = afterPos < 0 ? null : grid.colIdAt(afterPos);
    if (afterPos >= 0 && afterColId === null) {
      return;
    }
    notePersistenceMutation();
    pendingActionVerbRef.current = "inserted columns";
    client.send({
      kind: "insertCols",
      sheet: activeSheetRef.current,
      afterColId,
      count: 1,
    });
    editorRef.current?.focus();
  };

  const handleDeleteSelectedCols = () => {
    const grid = gridRef.current;
    const client = clientRef.current;
    if (!grid || !client || workerTerminatedRef.current) {
      return;
    }
    const cols = grid.selectedColRuns();
    if (!cols || cols.length === 0) {
      return;
    }
    notePersistenceMutation();
    pendingActionVerbRef.current = "deleted columns";
    client.send({
      kind: "deleteCols",
      sheet: activeSheetRef.current,
      cols,
    });
    editorRef.current?.focus();
  };

  const applyFormat = (patch: { numberFormat?: WireFormatPatch; style?: WireStylePatch }) => {
    const grid = gridRef.current;
    const client = clientRef.current;
    if (!grid || !client || workerTerminatedRef.current) {
      return;
    }
    const target = grid.selectedFormatTarget();
    if (!target) {
      return;
    }
    notePersistenceMutation();
    pendingActionVerbRef.current = "formatted cells";
    client.send({
      kind: "setFormat",
      sheet: activeSheetRef.current,
      target,
      ...patch,
    });
    editorRef.current?.focus();
  };

  const handleFreezeChange = (value: string) => {
    const client = clientRef.current;
    if (!client || workerTerminatedRef.current) {
      return;
    }
    let frozenRows = 0;
    let frozenCols = 0;
    if (value === "row1") {
      frozenRows = 1;
    } else if (value === "col1") {
      frozenCols = 1;
    } else if (value === "row1col1") {
      frozenRows = 1;
      frozenCols = 1;
    } else if (value === "active" && selection) {
      frozenRows = Math.min(32, selection.row);
      frozenCols = Math.min(32, selection.col);
      if (frozenRows === 0 && frozenCols === 0) {
        frozenRows = 1;
      }
    }
    notePersistenceMutation();
    pendingActionVerbRef.current = "updated freeze panes";
    client.send({
      kind: "setFreeze",
      sheet: activeSheetRef.current,
      frozenRows,
      frozenCols,
    });
    editorRef.current?.focus();
  };

  const handleSwitchSheet = (targetId: number) => {
    const grid = gridRef.current;
    const editor = editorRef.current;
    const client = clientRef.current;
    if (
      !grid ||
      !editor ||
      !client ||
      workerTerminatedRef.current ||
      targetId === activeSheetRef.current
    ) {
      return;
    }
    const session = editSessionRef.current;
    if (
      session?.source.startsWith("=") &&
      (session.pointPickedSpan !== null || session.canInsertRef || session.activeRefSpan !== null)
    ) {
      editor.clearSilent();
      client.send({ kind: "switchSheet", sheet: targetId });
      formulaBarRef.current?.focus();
      return;
    }
    if (editor.editing) {
      editor.commitExternal(false);
    }
    clearEditState(grid, editor);
    pendingActionVerbRef.current = "switched sheet";
    client.send({ kind: "switchSheet", sheet: targetId });
    editor.focus();
  };

  const handleContextClipboard = async (mode: "cut" | "copy" | "paste") => {
    const grid = gridRef.current;
    const client = clientRef.current;
    setContextMenu(null);
    if (!grid || !client || workerTerminatedRef.current) {
      return;
    }
    const range = grid.selectionRange;
    if (!range) {
      return;
    }
    if (mode === "cut" || mode === "copy") {
      client.send({
        kind: "copyRange",
        sheet: activeSheetRef.current,
        firstRow: range.firstRow,
        rowCount: range.endRow - range.firstRow,
        firstCol: range.firstCol,
        colCount: range.endCol - range.firstCol,
        cut: mode === "cut",
      });
      editorRef.current?.focus();
      return;
    }
    let tsv = "";
    let html: string | undefined;
    try {
      if (typeof navigator !== "undefined" && navigator.clipboard?.readText) {
        tsv = await navigator.clipboard.readText();
      }
    } catch {
      // Fall back to internal clipboard when browser blocks async clipboard read.
    }
    if (tsv.length === 0 && lastCopiedRef.current) {
      tsv = lastCopiedRef.current.tsv;
      html = lastCopiedRef.current.html;
    }
    if (tsv.length > 0 || html) {
      notePersistenceMutation();
      sendEvalEnv(client);
      client.send({
        kind: "pasteClipboard",
        sheet: activeSheetRef.current,
        destRow: range.firstRow,
        destRowCount: range.endRow - range.firstRow,
        destCol: range.firstCol,
        destColCount: range.endCol - range.firstCol,
        tsv,
        html,
      });
    }
    editorRef.current?.focus();
  };

  const handleTakeOver = (targetDocId: string) => {
    if (workerTerminatedRef.current || !clientRef.current) {
      startWorkerFnRef.current?.(targetDocId, { kind: "takeOverDoc", docId: targetDocId });
    } else {
      clientRef.current.sendHost({ kind: "takeOverDoc", docId: targetDocId });
    }
  };

  const handleCreateNewWorkbook = (title: string, template: "empty" | "sample") => {
    const newId = makeFreshDocId();
    notePersistenceMutation();
    setModalMode(null);
    setLockOverlay(null);
    if (workerTerminatedRef.current || !clientRef.current) {
      startWorkerFnRef.current?.(newId, { kind: "createDoc", docId: newId, title, template });
    } else {
      clientRef.current.sendHost({ kind: "createDoc", docId: newId, title, template });
    }
  };

  const handleOpenWorkbooksDrawer = () => {
    if (!workerTerminatedRef.current) {
      clientRef.current?.sendHost({ kind: "listDocs" });
    }
    refreshStorageEstimate();
    setModalMode({ kind: "workbooks" });
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

  const readySheet = engine.status === "ready" && lockOverlay === null ? engine.sheet : null;
  const canDeleteRows = selection !== null && selection.rowId !== null;
  const canDeleteCols = selection !== null && selection.colId !== null;
  const hasSelection = readySheet !== null && selection !== null;
  const activeStyle = matchingInspected?.style;
  const activeFormatCode = matchingInspected?.formatCode ?? "General";
  const currentFreezeValue = readySheet
    ? readySheet.frozenRows === 0 && readySheet.frozenCols === 0
      ? "none"
      : readySheet.frozenRows === 1 && readySheet.frozenCols === 0
        ? "row1"
        : readySheet.frozenRows === 0 && readySheet.frozenCols === 1
          ? "col1"
          : readySheet.frozenRows === 1 && readySheet.frozenCols === 1
            ? "row1col1"
            : "active"
    : "none";

  const textPalette = isDark ? DARK_PALETTE_TEXT : LIGHT_PALETTE_TEXT;
  const fillPalette = isDark ? DARK_PALETTE_FILL : LIGHT_PALETTE_FILL;

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
        <button
          type="button"
          className="toolbar-btn"
          data-testid="workbooks-btn"
          onClick={handleOpenWorkbooksDrawer}
          title="Manage local browser workbooks, import or export .rtcl / .csv"
        >
          Workbooks
        </button>
        {readySheet && (
          <span className="document" data-testid="document">
            {docTitle} · {integer.format(readySheet.rows)} rows × {readySheet.cols} columns ·{" "}
            {integer.format(readySheet.formulaCount)} formulas
          </span>
        )}
        <span className="titlebar-spacer" />
        <span className="storage-badge" data-testid="storage-badge">
          Local browser storage only · {storageUsedMb} MB used ·{" "}
          <button
            type="button"
            className="storage-export-link"
            data-testid="export-rtcl-btn"
            disabled={!readySheet}
            onClick={() => clientRef.current?.sendHost({ kind: "exportRtcl" })}
          >
            Export .rtcl
          </button>
        </span>
        <div className="toolbar">
          <button
            type="button"
            className="toolbar-btn"
            data-testid="undo-btn"
            disabled={!readySheet?.canUndo}
            onClick={() => {
              notePersistenceMutation();
              pendingActionVerbRef.current = "undid action";
              clientRef.current?.send({ kind: "undo" });
              editorRef.current?.focus();
            }}
            title={
              readySheet?.undoLabel ? `Undo ${readySheet.undoLabel} (Ctrl+Z)` : "Undo (Ctrl+Z)"
            }
          >
            ↶ Undo
          </button>
          <button
            type="button"
            className="toolbar-btn"
            data-testid="redo-btn"
            disabled={!readySheet?.canRedo}
            onClick={() => {
              notePersistenceMutation();
              pendingActionVerbRef.current = "redid action";
              clientRef.current?.send({ kind: "redo" });
              editorRef.current?.focus();
            }}
            title={
              readySheet?.redoLabel ? `Redo ${readySheet.redoLabel} (Ctrl+Y)` : "Redo (Ctrl+Y)"
            }
          >
            ↷ Redo
          </button>
          <button
            type="button"
            className="toolbar-btn"
            data-testid="insert-row-above"
            disabled={!readySheet}
            onClick={() => handleInsertRow(false)}
            title="Insert 1 row above the selected row (at row 1 if nothing is selected)"
          >
            + Insert Row Above
          </button>
          <button
            type="button"
            className="toolbar-btn"
            data-testid="delete-rows"
            disabled={!readySheet || !canDeleteRows}
            onClick={handleDeleteSelectedRows}
            title="Delete all rows spanned by the current selection"
          >
            Delete Selected Rows
          </button>
        </div>
      </header>

      <div className="formatbar" role="toolbar" aria-label="Cell formatting and freeze panes">
        <select
          className="fmt-select"
          data-testid="number-format-select"
          aria-label="Number format"
          disabled={!hasSelection}
          value={
            NUMBER_FORMAT_OPTIONS.some((o) => o.code === activeFormatCode)
              ? activeFormatCode
              : "General"
          }
          onChange={(e) => {
            const code = e.target.value;
            applyFormat({
              numberFormat: code === "General" ? { kind: "clear" } : { kind: "set", code },
            });
          }}
        >
          {NUMBER_FORMAT_OPTIONS.map((opt) => (
            <option key={opt.code} value={opt.code}>
              {opt.label}
            </option>
          ))}
        </select>

        <span className="fmt-divider" aria-hidden="true" />

        <button
          type="button"
          className={activeStyle?.bold ? "fmt-btn active" : "fmt-btn"}
          data-testid="fmt-bold"
          aria-pressed={activeStyle?.bold ?? false}
          disabled={!hasSelection}
          title="Bold"
          onClick={() => applyFormat({ style: { bold: !(activeStyle?.bold ?? false) } })}
        >
          <strong>B</strong>
        </button>
        <button
          type="button"
          className={activeStyle?.italic ? "fmt-btn active" : "fmt-btn"}
          data-testid="fmt-italic"
          aria-pressed={activeStyle?.italic ?? false}
          disabled={!hasSelection}
          title="Italic"
          onClick={() => applyFormat({ style: { italic: !(activeStyle?.italic ?? false) } })}
        >
          <em>I</em>
        </button>
        <button
          type="button"
          className={activeStyle?.strikethrough ? "fmt-btn active" : "fmt-btn"}
          data-testid="fmt-strike"
          aria-pressed={activeStyle?.strikethrough ?? false}
          disabled={!hasSelection}
          title="Strikethrough"
          onClick={() =>
            applyFormat({ style: { strikethrough: !(activeStyle?.strikethrough ?? false) } })
          }
        >
          <span style={{ textDecoration: "line-through" }}>S</span>
        </button>

        <span className="fmt-divider" aria-hidden="true" />

        {(["left", "center", "right"] as const satisfies readonly WireHorizontalAlign[]).map(
          (align) => {
            const isPressed = activeStyle?.align === align;
            const label = align === "left" ? "⫷" : align === "center" ? "≡" : "⫸";
            return (
              <button
                key={align}
                type="button"
                className={isPressed ? "fmt-btn active" : "fmt-btn"}
                data-testid={`align-${align}`}
                aria-pressed={isPressed}
                disabled={!hasSelection}
                title={`Align ${align}`}
                onClick={() => applyFormat({ style: { align: isPressed ? "auto" : align } })}
              >
                {label}
              </button>
            );
          },
        )}

        <span className="fmt-divider" aria-hidden="true" />

        <div className="color-popover-wrap">
          <button
            type="button"
            className="fmt-btn"
            data-testid="text-color-btn"
            disabled={!hasSelection}
            title="Text color"
            onClick={() => setColorPopover((cur) => (cur === "text" ? null : "text"))}
          >
            <span className="color-glyph">A</span>
            <span
              className="color-swatch-bar"
              style={{
                background:
                  activeStyle?.textColor?.kind === "palette"
                    ? (textPalette[activeStyle.textColor.slot & 7] ?? "currentColor")
                    : "currentColor",
              }}
            />
          </button>
          {colorPopover === "text" && (
            <div className="color-popover" data-testid="text-color-popover">
              <button
                type="button"
                className="color-clear-btn"
                data-testid="text-color-clear"
                onClick={() => {
                  setColorPopover(null);
                  applyFormat({ style: { textColor: { kind: "clear" } } });
                }}
              >
                Default text color
              </button>
              <div className="color-grid">
                {textPalette.map((hex, slot) => (
                  <button
                    key={hex}
                    type="button"
                    className="color-swatch"
                    data-testid={`text-color-slot-${slot}`}
                    style={{ background: hex }}
                    aria-label={`Text palette slot ${slot + 1}`}
                    onClick={() => {
                      setColorPopover(null);
                      applyFormat({
                        style: { textColor: { kind: "set", color: { kind: "palette", slot } } },
                      });
                    }}
                  />
                ))}
              </div>
            </div>
          )}
        </div>

        <div className="color-popover-wrap">
          <button
            type="button"
            className="fmt-btn"
            data-testid="fill-color-btn"
            disabled={!hasSelection}
            title="Fill color"
            onClick={() => setColorPopover((cur) => (cur === "fill" ? null : "fill"))}
          >
            <span className="color-glyph">▨</span>
            <span
              className="color-swatch-bar"
              style={{
                background:
                  activeStyle?.fillColor?.kind === "palette"
                    ? (fillPalette[activeStyle.fillColor.slot & 7] ?? "transparent")
                    : "var(--shell-line)",
              }}
            />
          </button>
          {colorPopover === "fill" && (
            <div className="color-popover" data-testid="fill-color-popover">
              <button
                type="button"
                className="color-clear-btn"
                data-testid="fill-color-clear"
                onClick={() => {
                  setColorPopover(null);
                  applyFormat({ style: { fillColor: { kind: "clear" } } });
                }}
              >
                No fill
              </button>
              <div className="color-grid">
                {fillPalette.map((hex, slot) => (
                  <button
                    key={hex}
                    type="button"
                    className="color-swatch"
                    data-testid={`fill-color-slot-${slot}`}
                    style={{ background: hex }}
                    aria-label={`Fill palette slot ${slot + 1}`}
                    onClick={() => {
                      setColorPopover(null);
                      applyFormat({
                        style: { fillColor: { kind: "set", color: { kind: "palette", slot } } },
                      });
                    }}
                  />
                ))}
              </div>
            </div>
          )}
        </div>

        <span className="fmt-divider" aria-hidden="true" />

        <button
          type="button"
          className="fmt-btn"
          data-testid="border-thin"
          disabled={!hasSelection}
          title="Thin borders"
          onClick={() =>
            applyFormat({
              style: {
                borderTop: "thin",
                borderRight: "thin",
                borderBottom: "thin",
                borderLeft: "thin",
              },
            })
          }
        >
          ⊞ Thin
        </button>
        <button
          type="button"
          className="fmt-btn"
          data-testid="border-medium"
          disabled={!hasSelection}
          title="Medium borders"
          onClick={() =>
            applyFormat({
              style: {
                borderTop: "medium",
                borderRight: "medium",
                borderBottom: "medium",
                borderLeft: "medium",
              },
            })
          }
        >
          ⊞ Med
        </button>
        <button
          type="button"
          className="fmt-btn"
          data-testid="border-none"
          disabled={!hasSelection}
          title="Clear borders"
          onClick={() =>
            applyFormat({
              style: {
                borderTop: "none",
                borderRight: "none",
                borderBottom: "none",
                borderLeft: "none",
              },
            })
          }
        >
          ⊟ Clear
        </button>

        <span className="fmt-divider" aria-hidden="true" />

        <select
          className="fmt-select"
          data-testid="freeze-select"
          aria-label="Freeze panes"
          disabled={!readySheet}
          value={currentFreezeValue}
          onChange={(e) => handleFreezeChange(e.target.value)}
        >
          <option value="none">No Freeze</option>
          <option value="row1">Freeze Top Row</option>
          <option value="col1">Freeze First Column</option>
          <option value="row1col1">Freeze Row 1 &amp; Col A</option>
          <option value="active">Freeze to Active Cell</option>
        </select>

        <span className="fmt-divider" aria-hidden="true" />

        <button
          type="button"
          className="fmt-btn"
          data-testid="insert-col-left"
          disabled={!readySheet}
          title="Insert 1 column to the left of selection"
          onClick={() => handleInsertCol(false)}
        >
          + Col
        </button>
        <button
          type="button"
          className="fmt-btn"
          data-testid="delete-cols"
          disabled={!readySheet || !canDeleteCols}
          title="Delete selected columns"
          onClick={handleDeleteSelectedCols}
        >
          − Col
        </button>
      </div>

      {quotaBanner && (
        <div className="rejected-banner" role="alert" data-testid="quota-banner">
          <span>{quotaBanner}</span>
          <div className="banner-actions">
            <button
              type="button"
              className="tip-action"
              onClick={() => clientRef.current?.sendHost({ kind: "exportRtcl" })}
            >
              Export .rtcl
            </button>
            <button type="button" className="tip-action" onClick={handleOpenWorkbooksDrawer}>
              Manage Workbooks
            </button>
            <button
              type="button"
              className="tip-close"
              aria-label="Dismiss storage warning"
              onClick={() => setQuotaBanner(null)}
            >
              ×
            </button>
          </div>
        </div>
      )}

      {swUpdateReady && (
        <div className="sw-update-banner" role="status" data-testid="sw-update-banner">
          <span>
            A new version of Reticula is ready. Confirming flushes all open tabs and reloads safely.
          </span>
          <div className="quota-banner-actions">
            <button
              type="button"
              className="tip-action"
              data-testid="sw-update-btn"
              disabled={swUpdating}
              onClick={() => {
                setSwUpdating(true);
                void swClientRef.current?.applyUpdate().catch(() => setSwUpdating(false));
              }}
            >
              {swUpdating ? "Updating…" : "Update now"}
            </button>
            <button
              type="button"
              className="tip-close"
              aria-label="Dismiss update notification"
              onClick={() => setSwUpdateReady(false)}
            >
              ×
            </button>
          </div>
        </div>
      )}

      {readySheet?.recalcFrozen && (
        <div className="frozen-recalc-banner" role="status" data-testid="recalc-frozen-banner">
          <span>
            Safe Mode opened this workbook with automatic recalculation paused so you can inspect or
            edit formulas safely.
          </span>
          <button
            type="button"
            className="tip-action"
            data-testid="resume-recalc-btn"
            onClick={() => clientRef.current?.send({ kind: "resumeRecalc" })}
          >
            Resume Recalculation
          </button>
        </div>
      )}

      {showTip && readySheet && readySheet.seed !== 0 && readySheet.sheet === 0 && (
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

      <FormulaBar
        ref={formulaBarRef}
        nameBoxText={nameBoxText}
        value={formulaBarValue}
        placeholder={
          selection || editSessionRef.current
            ? "Enter a value or =formula (Tab to autocomplete, F4 to toggle $)"
            : "Select a cell to edit"
        }
        disabled={readySheet === null || (selection === null && !editSessionRef.current)}
        isEditing={editingText !== null}
        analysis={formulaAnalysis?.analysis ?? null}
        analysisSource={formulaAnalysis?.source ?? null}
        getEditSource={getEditSource}
        onChange={handleFormulaBarChange}
        onCommit={handleFormulaBarCommit}
        onCancel={handleFormulaBarCancel}
        onToggleDollar={(text, cursorUtf16) => {
          clientRef.current?.send({ kind: "toggleFormulaDollar", source: text, cursorUtf16 });
        }}
        onAcceptCompletion={handleAcceptCompletion}
      />

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
        {lockOverlay && (
          <div className="lock-overlay" role="dialog" aria-modal="true" data-testid="lock-overlay">
            <div className="lock-card">
              <h2>
                {lockOverlay.kind === "blocked"
                  ? "This local workbook is open in another tab"
                  : lockOverlay.kind === "takingOver"
                    ? "Moving workbook to this tab…"
                    : "Workbook moved to another tab"}
              </h2>
              <p data-testid="lock-overlay-message">
                {lockOverlay.kind === "blocked"
                  ? `“${lockOverlay.title}” is exclusively locked by another browser tab to keep its OPFS edit log consistent.`
                  : lockOverlay.kind === "takingOver"
                    ? lockOverlay.waitingForFile
                      ? "Waiting for the other tab to release the file — if the other tab is unresponsive, please close it."
                      : "Asking the other tab to flush and release the workbook…"
                    : lockOverlay.evicted
                      ? "Another tab took over this workbook because this tab did not release it in time."
                      : "This tab flushed its changes and handed the workbook over to another tab."}
              </p>
              {lockOverlay.kind !== "takingOver" && (
                <div className="lock-actions">
                  <button
                    type="button"
                    className="toolbar-btn primary-btn"
                    data-testid="take-over-btn"
                    onClick={() => handleTakeOver(lockOverlay.docId)}
                  >
                    Open here (Take over)
                  </button>
                  <button
                    type="button"
                    className="toolbar-btn"
                    data-testid="new-workbook-from-lock-btn"
                    onClick={() => handleCreateNewWorkbook("Untitled workbook", "empty")}
                  >
                    New local workbook
                  </button>
                </div>
              )}
            </div>
          </div>
        )}
        {engine.status === "failed" && (
          <div className="failure" role="alert">
            <p>The engine has stopped. Reload the page to start again.</p>
            <p className="detail" data-testid="engine-error">
              {engine.message}
            </p>
          </div>
        )}
      </div>

      {readySheet && (
        <div className="sheetbar" role="tablist" aria-label="Worksheets" data-testid="sheet-tabs">
          {readySheet.sheets.map((tab) => {
            const isActive = tab.id === readySheet.sheet;
            const isRenaming = renamingSheet?.id === tab.id;
            if (isRenaming) {
              return (
                <input
                  key={tab.id}
                  type="text"
                  className="sheet-rename-input"
                  data-testid="rename-sheet-input"
                  value={renamingSheet.draft}
                  maxLength={31}
                  // biome-ignore lint/a11y/noAutofocus: inline tab rename requires immediate focus
                  autoFocus
                  onChange={(e) => setRenamingSheet({ id: tab.id, draft: e.target.value })}
                  onBlur={() => {
                    const trimmed = renamingSheet.draft.trim();
                    setRenamingSheet(null);
                    if (trimmed && trimmed !== tab.name) {
                      notePersistenceMutation();
                      pendingActionVerbRef.current = "renamed sheet";
                      clientRef.current?.send({
                        kind: "renameSheet",
                        sheet: tab.id,
                        name: trimmed,
                      });
                    }
                  }}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") {
                      e.preventDefault();
                      const trimmed = renamingSheet.draft.trim();
                      setRenamingSheet(null);
                      if (trimmed && trimmed !== tab.name) {
                        notePersistenceMutation();
                        pendingActionVerbRef.current = "renamed sheet";
                        clientRef.current?.send({
                          kind: "renameSheet",
                          sheet: tab.id,
                          name: trimmed,
                        });
                      }
                      editorRef.current?.focus();
                    } else if (e.key === "Escape") {
                      e.preventDefault();
                      setRenamingSheet(null);
                      editorRef.current?.focus();
                    }
                  }}
                />
              );
            }
            return (
              <button
                key={tab.id}
                type="button"
                role="tab"
                aria-selected={isActive}
                className={isActive ? "sheet-tab active" : "sheet-tab"}
                data-testid={`sheet-tab-${tab.id}`}
                onClick={() => handleSwitchSheet(tab.id)}
                onDoubleClick={() => setRenamingSheet({ id: tab.id, draft: tab.name })}
                onContextMenu={(e) => {
                  e.preventDefault();
                  setColorPopover(null);
                  setContextMenu({
                    kind: "tab",
                    clientX: e.clientX,
                    clientY: e.clientY,
                    tab,
                  });
                }}
              >
                {tab.name}
              </button>
            );
          })}
          <button
            type="button"
            className="sheet-add-btn"
            data-testid="add-sheet-btn"
            title="Add worksheet"
            aria-label="Add worksheet"
            onClick={() => {
              notePersistenceMutation();
              pendingActionVerbRef.current = "added sheet";
              clientRef.current?.send({
                kind: "addSheet",
                afterSheet: readySheet.sheet,
              });
              editorRef.current?.focus();
            }}
          >
            +
          </button>
        </div>
      )}

      <footer className="statusbar">
        <span data-testid="engine-status">
          {lockOverlay
            ? lockOverlay.kind
            : modalMode?.kind === "safeMode"
              ? "safeMode"
              : engine.status}
        </span>
        {engine.status === "starting" && !lockOverlay && !modalMode && (
          <span>Opening local workbook…</span>
        )}
        {readySheet && engine.status === "ready" && (
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
            {readySheet.seed !== 0 && (
              <span title="The sample is computed from this seed, not downloaded">
                seed {readySheet.seed}
              </span>
            )}
            <span data-testid="generation" title="Time the engine took to generate or open">
              generated in {milliseconds.format(readySheet.generationMs)} ms
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

      {contextMenu && (
        <div
          className="context-menu"
          role="menu"
          data-testid="context-menu"
          style={{
            left: `${Math.min(window.innerWidth - 208, Math.max(8, contextMenu.kind === "grid" ? contextMenu.target.clientX : contextMenu.clientX))}px`,
            top: `${Math.min(window.innerHeight - 260, Math.max(8, contextMenu.kind === "grid" ? contextMenu.target.clientY : contextMenu.clientY))}px`,
          }}
        >
          {contextMenu.kind === "grid" ? (
            <>
              <button
                type="button"
                role="menuitem"
                className="ctx-item"
                data-testid="ctx-cut"
                onClick={() => void handleContextClipboard("cut")}
              >
                Cut <kbd>Ctrl+X</kbd>
              </button>
              <button
                type="button"
                role="menuitem"
                className="ctx-item"
                data-testid="ctx-copy"
                onClick={() => void handleContextClipboard("copy")}
              >
                Copy <kbd>Ctrl+C</kbd>
              </button>
              <button
                type="button"
                role="menuitem"
                className="ctx-item"
                data-testid="ctx-paste"
                onClick={() => void handleContextClipboard("paste")}
              >
                Paste <kbd>Ctrl+V</kbd>
              </button>
              <button
                type="button"
                role="menuitem"
                className="ctx-item"
                data-testid="ctx-clear"
                onClick={() => {
                  setContextMenu(null);
                  const runs = gridRef.current?.selectedCellRuns();
                  if (runs) {
                    notePersistenceMutation();
                    pendingActionVerbRef.current = "cleared cells";
                    clientRef.current?.send({
                      kind: "clearCells",
                      sheet: activeSheetRef.current,
                      rows: runs.rows,
                      cols: runs.cols,
                    });
                  }
                  editorRef.current?.focus();
                }}
              >
                Clear Contents <kbd>Del</kbd>
              </button>
              <div className="ctx-sep" />
              <button
                type="button"
                role="menuitem"
                className="ctx-item"
                data-testid="ctx-insert-row-above"
                onClick={() => {
                  setContextMenu(null);
                  handleInsertRow(false);
                }}
              >
                Insert Row Above
              </button>
              <button
                type="button"
                role="menuitem"
                className="ctx-item"
                data-testid="ctx-insert-row-below"
                onClick={() => {
                  setContextMenu(null);
                  handleInsertRow(true);
                }}
              >
                Insert Row Below
              </button>
              <button
                type="button"
                role="menuitem"
                className="ctx-item"
                data-testid="ctx-delete-rows"
                disabled={!canDeleteRows}
                onClick={() => {
                  setContextMenu(null);
                  handleDeleteSelectedRows();
                }}
              >
                Delete Selected Rows
              </button>
              <div className="ctx-sep" />
              <button
                type="button"
                role="menuitem"
                className="ctx-item"
                data-testid="ctx-insert-col-left"
                onClick={() => {
                  setContextMenu(null);
                  handleInsertCol(false);
                }}
              >
                Insert Column Left
              </button>
              <button
                type="button"
                role="menuitem"
                className="ctx-item"
                data-testid="ctx-insert-col-right"
                onClick={() => {
                  setContextMenu(null);
                  handleInsertCol(true);
                }}
              >
                Insert Column Right
              </button>
              <button
                type="button"
                role="menuitem"
                className="ctx-item"
                data-testid="ctx-delete-cols"
                disabled={!canDeleteCols}
                onClick={() => {
                  setContextMenu(null);
                  handleDeleteSelectedCols();
                }}
              >
                Delete Selected Columns
              </button>
            </>
          ) : (
            <>
              <button
                type="button"
                role="menuitem"
                className="ctx-item"
                data-testid="ctx-rename-sheet"
                onClick={() => {
                  const { tab } = contextMenu;
                  setContextMenu(null);
                  setRenamingSheet({ id: tab.id, draft: tab.name });
                }}
              >
                Rename Sheet…
              </button>
              {readySheet &&
                (() => {
                  const idx = readySheet.sheets.findIndex((s) => s.id === contextMenu.tab.id);
                  const canLeft = idx > 0;
                  const canRight = idx >= 0 && idx < readySheet.sheets.length - 1;
                  return (
                    <>
                      <button
                        type="button"
                        role="menuitem"
                        className="ctx-item"
                        data-testid="ctx-move-sheet-left"
                        disabled={!canLeft}
                        onClick={() => {
                          const targetTab = contextMenu.tab;
                          setContextMenu(null);
                          const afterSheet = idx >= 2 ? readySheet.sheets[idx - 2]?.id : undefined;
                          notePersistenceMutation();
                          pendingActionVerbRef.current = "reordered sheets";
                          clientRef.current?.send({
                            kind: "moveSheet",
                            sheet: targetTab.id,
                            afterSheet,
                          });
                        }}
                      >
                        Move Left
                      </button>
                      <button
                        type="button"
                        role="menuitem"
                        className="ctx-item"
                        data-testid="ctx-move-sheet-right"
                        disabled={!canRight}
                        onClick={() => {
                          const targetTab = contextMenu.tab;
                          setContextMenu(null);
                          const nextTab = readySheet.sheets[idx + 1];
                          if (nextTab) {
                            notePersistenceMutation();
                            pendingActionVerbRef.current = "reordered sheets";
                            clientRef.current?.send({
                              kind: "moveSheet",
                              sheet: targetTab.id,
                              afterSheet: nextTab.id,
                            });
                          }
                        }}
                      >
                        Move Right
                      </button>
                      <div className="ctx-sep" />
                      <button
                        type="button"
                        role="menuitem"
                        className="ctx-item danger"
                        data-testid="ctx-delete-sheet"
                        disabled={readySheet.sheets.length <= 1}
                        onClick={() => {
                          const targetTab = contextMenu.tab;
                          setContextMenu(null);
                          notePersistenceMutation();
                          pendingActionVerbRef.current = "deleted sheet";
                          clientRef.current?.send({
                            kind: "deleteSheet",
                            sheet: targetTab.id,
                          });
                        }}
                      >
                        Delete Sheet
                      </button>
                    </>
                  );
                })()}
            </>
          )}
        </div>
      )}

      {modalMode && (
        <Suspense fallback={null}>
          <LazyLocalModals
            mode={modalMode}
            activeDocId={docId}
            docs={catalogDocs}
            storageUsedMb={storageUsedMb}
            csvProgress={csvProgress}
            onClose={() => setModalMode(null)}
            onOpenDoc={(nextId, nextTitle) => {
              setModalMode(null);
              if (workerTerminatedRef.current || !clientRef.current) {
                startWorkerFnRef.current?.(nextId, {
                  kind: "openDoc",
                  docId: nextId,
                  title: nextTitle,
                });
              } else {
                clientRef.current.sendHost({ kind: "openDoc", docId: nextId, title: nextTitle });
              }
            }}
            onCreateDoc={handleCreateNewWorkbook}
            onRenameDoc={(id, title) => {
              notePersistenceMutation();
              clientRef.current?.sendHost({ kind: "renameDoc", docId: id, title });
            }}
            onDeleteDoc={(id) => {
              notePersistenceMutation();
              clientRef.current?.sendHost({ kind: "deleteDoc", docId: id });
            }}
            onExportRtcl={() => clientRef.current?.sendHost({ kind: "exportRtcl" })}
            onExportCsv={() =>
              clientRef.current?.sendHost({ kind: "exportCsv", sheet: activeSheetRef.current })
            }
            onImportRtcl={(title, bytes) => {
              const newId = makeFreshDocId();
              notePersistenceMutation();
              setModalMode(null);
              clientRef.current?.sendHost({ kind: "importRtcl", docId: newId, title, bytes }, [
                bytes,
              ]);
            }}
            onImportCsv={(title, sheetName, bytes) => {
              const newId = makeFreshDocId();
              notePersistenceMutation();
              clientRef.current?.sendHost(
                { kind: "importCsv", docId: newId, title, sheetName, bytes },
                [bytes],
              );
            }}
            onImportRejected={(reason) => {
              setModalMode(null);
              setRejectedReason(reason);
            }}
            onSafeModeOpenFrozen={(targetId) => {
              setModalMode(null);
              clientRef.current?.sendHost({
                kind: "safeModeRecover",
                docId: targetId,
                mode: { type: "openFrozen" },
              });
            }}
            onSafeModeSkipTail={(targetId, skipCount, salvageMid) => {
              setModalMode(null);
              clientRef.current?.sendHost({
                kind: "safeModeRecover",
                docId: targetId,
                mode: { type: "skipTail", skipCount, salvageMid },
              });
            }}
          />
        </Suspense>
      )}
    </div>
  );
}
