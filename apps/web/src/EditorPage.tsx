import {
  CellEditor,
  columnName,
  Grid,
  type GridStats,
  type RemotePresenceOverlay,
  type Selection,
  type Span,
} from "@reticula/grid";
import type {
  CollabEvent,
  EngineCommand,
  SheetInfo,
  SheetTabInfo,
  WireCellStyle,
  WireCompletionItem,
  WireFormulaAnalysis,
  WireSearchMatch,
} from "@reticula/protocol";
import { type DragEvent, lazy, Suspense, useCallback, useEffect, useRef, useState } from "react";
import type { CollabWorkbookTarget } from "./collab/CollabShell.tsx";
import type {
  CopiedSnapshot,
  EditorHandle,
  EditorState,
  MutateOptions,
  ShownTable,
  TableAsk,
  TableView,
} from "./commands/handle.ts";
import { runCommand } from "./commands/run.ts";
import type { Activity, SheetAction } from "./editor/activity.ts";
import type { ImportProgress } from "./editor/Banners.tsx";
import type { CellInspection } from "./editor/cell-announcer.ts";
import { DemoTip } from "./editor/DemoTip.tsx";
import { FormatToolbar } from "./editor/FormatToolbar.tsx";
import { useGridStrings } from "./editor/grid-strings.ts";
import { type CellIds, HeldCommits } from "./editor/held-commits.ts";
import type { KeptEdit } from "./editor/kept-edit.ts";
import {
  engineText,
  laterNow,
  lazyPart,
  loadLater,
  loadLaterWhenIdle,
} from "./editor/load-later.ts";
import { NameBox } from "./editor/NameBox.tsx";
import { SheetTabs } from "./editor/SheetTabs.tsx";
import { StatusBar } from "./editor/StatusBar.tsx";
import { TitleBar } from "./editor/TitleBar.tsx";
import { isTouchMode, useTouchMode } from "./editor/touch-mode.ts";
import type { EditorDialog, EditorMenu, EngineState, LockOverlayState } from "./editor/types.ts";
import { ViewMemory } from "./editor/view-state.ts";
import { ZoomSignal } from "./editor/ZoomControl.tsx";
import {
  type EngineClient,
  type EngineMessage,
  type HostCommand,
  type SafeModeInfo,
  startEngine,
} from "./engine/client.ts";
import type { CollabTransportHandle } from "./engine/collab-transport.ts";
import type { StorageBackendKind, WorkbookCatalogEntry } from "./engine/storage.ts";
import { FormulaBar, type FormulaBarHandle } from "./FormulaBar.tsx";
import {
  type CollabHookStatus,
  type HookDocStatus,
  hooksRequested,
  touchOverride,
} from "./hooks.ts";
import type { HookHost } from "./hooks-install.ts";
import { type Message, t } from "./i18n/index.ts";
import { commandFor, focusScope, swallowContextMenu } from "./keymap.ts";
import type { LocalModalMode } from "./LocalModals.tsx";
import type { M3aModalEvent, M3aToolMode } from "./M3aToolsModal.tsx";
import { rendererPreference } from "./renderer.ts";
import type { SwClientHandle } from "./sw-client.ts";
import type { TableEntry } from "./tables/catalog.ts";
import { announce } from "./ui/announcer.ts";
import { useMedia } from "./ui/media.ts";
import { useOpener } from "./ui/opener.ts";
import { loadPopup } from "./ui/Popover.tsx";

const LazyMobileFallback = lazy(() => import("./editor/MobileFallback.tsx"));
const LazyLocalModals = lazy(() => import("./LocalModals.tsx"));
const LazyCollabHost = lazy(() => import("./collab/CollabHost.tsx"));
const LazyM3aToolsModal = lazy(() => import("./M3aToolsModal.tsx"));
const LazyAiHost = lazy(() => import("./ai/AiHost.tsx"));
const LazyBanners = lazyPart(({ Banners }) => Banners);
const LazyLockOverlay = lazyPart(({ LockOverlay }) => LockOverlay);
const LazyMenus = lazyPart(({ EditorMenus }) => EditorMenus);
const LazyDialogs = lazyPart(({ EditorDialogs }) => EditorDialogs);
const LazyAnnouncements = lazyPart(({ CellAnnouncements }) => CellAnnouncements);
const LazyTableDock = lazyPart(({ TableDock }) => TableDock);
const LazyTouchBar = lazyPart(({ TouchBar }) => TouchBar);

interface InspectedState extends CellInspection {
  readonly sheet: number;
  readonly rowId: string;
  readonly colId: string;
  readonly formatCode: string;
  readonly style: WireCellStyle;
  readonly isSpillShadow: boolean;
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
  editRev: number;
}

const DEFAULT_DOC_ID = "sample-1m";
const STATUS_INTERVAL_MS = 500;
const YIELD_WATCHDOG_MS = 500;

const MARKS = {
  shell: "reticula:shell",
  engineReady: "reticula:engine-ready",
  firstScreen: "reticula:first-screen",
} as const;

function computeLocalExcelSerialNow(): number {
  const now = new Date();
  const localMs = now.getTime() - now.getTimezoneOffset() * 60_000;
  return 25569 + localMs / 86_400_000;
}

function nextEvalSeed(): string {
  return String(crypto.getRandomValues(new BigUint64Array(1))[0]);
}

function sendEvalEnv(client: EngineClient): void {
  client.send({
    kind: "setEvalEnv",
    nowSerial: computeLocalExcelSerialNow(),
    seed: nextEvalSeed(),
    timestampMs: Date.now(),
  });
}

function readInitialUrlDoc(): {
  docId: string;
  safeRequested: boolean;
  collabId: string | null;
  joinToken: string | null;
  shareToken: string | null;
} {
  const params = new URLSearchParams(window.location.search);
  const rawDoc = params.get("doc")?.trim();
  const safeDoc = rawDoc && /^[a-zA-Z0-9._-]+$/.test(rawDoc) ? rawDoc : DEFAULT_DOC_ID;
  const rawCollab = params.get("collab")?.trim() ?? null;
  const collabId = rawCollab && /^[a-zA-Z0-9-]+$/.test(rawCollab) ? rawCollab : null;
  const joinMatch = /^\/join\/([a-zA-Z0-9_-]+)\/?$/.exec(window.location.pathname);
  const joinToken = joinMatch?.[1] ?? null;
  const rawShare = params.get("share")?.trim() ?? null;
  const shareToken = rawShare && /^[a-zA-Z0-9_-]+$/.test(rawShare) ? rawShare : null;
  return {
    docId: safeDoc,
    safeRequested: params.get("safe") === "1",
    collabId,
    joinToken,
    shareToken,
  };
}

/** Whether something typed before an update waits to be had back (`editor/kept-edit.ts`). */
function hasKeptEdit(): boolean {
  try {
    return sessionStorage.getItem("reticula:kept-edit") !== null;
  } catch {
    return false;
  }
}

function syncUrlForDoc(docId: string): void {
  const url = new URL(window.location.href);
  url.searchParams.delete("safe");
  url.searchParams.delete("collab");
  url.searchParams.delete("share");
  url.pathname = "/app";
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

function makeFreshDocId(): string {
  return `wb-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 6)}`;
}

export interface EditorPageProps {
  readonly onNavigate: (path: string) => void;
}

export function EditorPage({ onNavigate }: EditorPageProps) {
  const isNarrow = useMedia("(max-width: 767px)");
  const [forceEditor, setForceEditor] = useState(false);
  const isOpen = useRef(false);

  if (
    isNarrow &&
    !forceEditor &&
    // A phone that is turned keeps what it shows.
    !isOpen.current &&
    (!hooksRequested() || touchOverride()) &&
    // Who follows a link to a workbook wants to see that.
    !(isTouchMode() && readInitialUrlDoc().joinToken)
  ) {
    return (
      <Suspense fallback={null}>
        <LazyMobileFallback onNavigate={onNavigate} onOpenAnyway={() => setForceEditor(true)} />
      </Suspense>
    );
  }

  isOpen.current ||= isTouchMode();
  return <DesktopSpreadsheet onNavigate={onNavigate} />;
}

function DesktopSpreadsheet({ onNavigate }: EditorPageProps) {
  const initialRoute = useRef(readInitialUrlDoc());
  const tabIdRef = useRef<string>(Math.random().toString(36).slice(2, 10));

  const hostRef = useRef<HTMLDivElement>(null);
  const gridRef = useRef<Grid | null>(null);
  const editorRef = useRef<CellEditor | null>(null);
  const formulaBarRef = useRef<FormulaBarHandle | null>(null);
  const clientRef = useRef<EngineClient | null>(null);
  const startWorkerFnRef = useRef<
    | ((docId: string, initialCmd: HostCommand, nextCollab?: CollabWorkbookTarget | null) => void)
    | null
  >(null);
  const swClientRef = useRef<SwClientHandle | null>(null);
  const collabTransportRef = useRef<CollabTransportHandle | null>(null);
  const collabTargetRef = useRef<CollabWorkbookTarget | null>(null);
  /** The workbook that is open, as what was typed in it is kept by. */
  const editDocKey = useCallback(
    () =>
      collabTargetRef.current
        ? `cloud:${collabTargetRef.current.docId}`
        : `local:${activeDocIdRef.current}`,
    [],
  );
  const collabConnRef = useRef<CollabHookStatus["connection"]>("offline");
  const collabReplicaRef = useRef<number | null>(null);
  const collabPendingRef = useRef<number>(0);
  const collabSlotRef = useRef<string | null>(null);
  const collabPresencesRef = useRef<readonly RemotePresenceOverlay[]>([]);
  const collabManualOfflineRef = useRef<boolean>(false);
  const kernelVersionRef = useRef<string>("0.0.0");
  const presenceTimerRef = useRef<number>(0);
  const lastSelectionRef = useRef<Selection | null>(null);

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
  /** The sheet that has been asked for, until it is in view. */
  const askedSheetRef = useRef<number | null>(null);
  const inspectedRef = useRef<InspectedState | null>(null);
  const editSessionRef = useRef<ActiveEditSession | null>(null);
  const analysisRef = useRef<WireFormulaAnalysis | null>(null);
  const lastCopiedRef = useRef<CopiedSnapshot | null>(null);
  const pendingActionRef = useRef<SheetAction>("updated");
  /** What to do once a sheet is in view. */
  const pendingJumpRef = useRef<{
    sheet: number;
    then: (grid: Grid, arrived: SheetInfo) => void;
  } | null>(null);
  const heldRef = useRef<HeldCommits | null>(null);
  const interceptorsRef = useRef(new Set<(message: EngineMessage) => boolean>());
  const internalsRef = useRef<HookHost["internals"]["current"]>(null);

  const [engine, setEngine] = useState<EngineState>({ status: "starting" });
  const [docId, setDocId] = useState<string>(initialRoute.current.docId);
  /** Tells a workbook from the one before it that had its name. */
  const [opening, setOpening] = useState(0);
  const [docTitle, setDocTitle] = useState<string>(activeDocTitleRef.current);
  const [catalogDocs, setCatalogDocs] = useState<readonly WorkbookCatalogEntry[]>([]);
  const [lockOverlay, setLockOverlay] = useState<LockOverlayState | null>(null);
  const [modalMode, setModalMode] = useState<LocalModalMode | null>(null);
  const [m3aTool, setM3aTool] = useState<M3aToolMode | null>(null);
  const [m3aEvent, setM3aEvent] = useState<M3aModalEvent | null>(null);
  const [quotaBanner, setQuotaBanner] = useState<string | Message | null>(null);
  const [swUpdateReady, setSwUpdateReady] = useState(false);
  const [csvProgress, setCsvProgress] = useState<ImportProgress | null>(null);
  const [storageUsedMb, setStorageUsedMb] = useState<string>("0.0");

  const [stats, setStats] = useState<GridStats | null>(null);
  const [zoomSignal] = useState(() => new ZoomSignal());
  const [selection, setSelection] = useState<Selection | null>(null);
  const [inspected, setInspected] = useState<InspectedState | null>(null);
  const [editingText, setEditingText] = useState<string | null>(null);
  const [formulaAnalysis, setFormulaAnalysis] = useState<{
    readonly source: string;
    readonly analysis: WireFormulaAnalysis;
  } | null>(null);
  const [activity, setActivity] = useState<Activity | null>(null);
  const [rejectedReason, setRejectedReason] = useState<string | Message | null>(null);
  const [menu, setMenu] = useState<EditorMenu | null>(null);
  const [dialog, setDialog] = useState<EditorDialog | null>(null);
  const [nameBoxEditing, setNameBoxEditing] = useState(false);
  /** Whether what the first screen does without has been loaded. */
  const [isLaterReady, setIsLaterReady] = useState(false);
  const [renamingSheet, setRenamingSheet] = useState<number | null>(null);
  const [collabMenuOpen, setCollabMenuOpen] = useState(false);
  const [collabTarget, setCollabTarget] = useState<CollabWorkbookTarget | null>(null);
  const [collabConn, setCollabConn] = useState<CollabHookStatus["connection"]>("offline");
  const [collabPending, setCollabPending] = useState(0);
  const [collabPresences, setCollabPresences] = useState<readonly RemotePresenceOverlay[]>([]);
  const [collabNotice, setCollabNotice] = useState<string | null>(null);
  const [historyOpen, setHistoryOpen] = useState(false);
  const [historyPreviewSeq, setHistoryPreviewSeq] = useState<string | null>(null);
  const [historySeekMs, setHistorySeekMs] = useState<number | null>(null);
  const [aiOpen, setAiOpen] = useState(false);
  const [draftPreview, setDraftPreview] = useState(false);
  /** The data tables there are, and the one in view in place of the worksheet. */
  const [tables, setTables] = useState<readonly TableEntry[]>([]);
  const [table, setTable] = useState<ShownTable | null>(null);
  const tableRef = useRef<string | null>(null);
  const [tableView, setTableView] = useState<TableView | null>(null);
  const [tableAsk, setTableAsk] = useState<TableAsk | null>(null);
  const touchMode = useTouchMode(() => {
    actionsRef.current.commitPending();
    editorRef.current?.refreshAttributes();
  });
  const effectiveReadOnly =
    touchMode ||
    collabTarget?.role === "viewer" ||
    historyPreviewSeq !== null ||
    draftPreview ||
    lockOverlay !== null ||
    csvProgress !== null;
  const effectiveReadOnlyRef = useRef(effectiveReadOnly);
  effectiveReadOnlyRef.current = effectiveReadOnly;
  const [unsyncedRescue, setUnsyncedRescue] = useState<{
    readonly localDocId: string;
    readonly title: string;
  } | null>(null);

  const sendPresenceNow = useCallback((sheetOverride?: number) => {
    const client = clientRef.current;
    if (
      !client ||
      workerTerminatedRef.current ||
      !collabTargetRef.current ||
      tableRef.current !== null
    ) {
      return;
    }
    const sel = lastSelectionRef.current;
    const sheet = sheetOverride ?? activeSheetRef.current;
    client.sendSync({
      kind: "localPresence",
      sheet,
      cursorRow: sel?.row ?? 0,
      cursorCol: sel?.col ?? 0,
      selection: sel
        ? {
            firstRow: sel.range.firstRow,
            endRow: sel.range.endRow,
            firstCol: sel.range.firstCol,
            endCol: sel.range.endCol,
          }
        : undefined,
    });
  }, []);

  const schedulePresenceSend = useCallback(() => {
    if (!collabTargetRef.current || presenceTimerRef.current !== 0) {
      return;
    }
    presenceTimerRef.current = window.setTimeout(() => {
      presenceTimerRef.current = 0;
      sendPresenceNow();
    }, 60);
  }, [sendPresenceNow]);

  const refreshStorageEstimate = useCallback(() => {
    if (!navigator.storage?.estimate) {
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
      if (navigator.storage?.persist) {
        void navigator.storage.persist().catch(() => {});
      }
    }
    refreshStorageEstimate();
  }, [refreshStorageEstimate]);

  const send = useCallback((command: EngineCommand): boolean => {
    if (!clientRef.current || workerTerminatedRef.current) {
      return false;
    }
    clientRef.current.send(command);
    return true;
  }, []);

  /** Sends a change, if anything may be changed. */
  const mutate = useCallback(
    (command: EngineCommand, { action, computes = false }: MutateOptions = {}): boolean => {
      const client = clientRef.current;
      if (!client || workerTerminatedRef.current || effectiveReadOnlyRef.current) {
        return false;
      }
      notePersistenceMutation();
      // What is held goes by positions, and these change them without a change of the sheet's size.
      if (/^move[RC]|do$/.test(command.kind)) {
        heldRef.current?.clear();
      }
      if (action) {
        pendingActionRef.current = action;
      }
      if (computes) {
        sendEvalEnv(client);
      }
      client.send(command);
      // What is known of the selected cell may not be so any more.
      const known = inspectedRef.current;
      if (known) {
        inspectedRef.current = null;
        const { sheet, rowId, colId } = known;
        client.send({ kind: "inspectCell", sheet, rowId, colId });
      }
      return true;
    },
    [notePersistenceMutation],
  );

  const openWith = useCallback((docId: string, command: HostCommand, isFresh = false) => {
    heldRef.current?.clear();
    if (isFresh || workerTerminatedRef.current || !clientRef.current) {
      startWorkerFnRef.current?.(docId, command);
    } else {
      clientRef.current.sendHost(command);
    }
  }, []);

  const askTables = useCallback((ask: TableAsk) => {
    setIsLaterReady(true);
    setTableAsk(ask);
  }, []);

  const openCollab = useCallback((target: CollabWorkbookTarget) => {
    const { userId, docId, branchId, title, role } = target;
    const online = !collabManualOfflineRef.current && navigator.onLine !== false;
    startWorkerFnRef.current?.(
      docId,
      { kind: "openCollab", userId, docId, branchId, title, online, readOnly: role === "viewer" },
      target,
    );
  }, []);

  /** Says why something was not done, whatever dialog is open. */
  const reject = useCallback((reason: string | Message) => {
    setRejectedReason(reason);
    announce(engineText(reason), { politeness: "assertive" });
  }, []);

  const handleM3aSend = useCallback((cmd: EngineCommand) => {
    clientRef.current?.send(cmd);
  }, []);

  const handleCloseM3aTool = useCallback(() => setM3aTool(null), []);

  /** Where the focus goes that has no other place: the grid, or what is in front of it. */
  const focusGrid = useCallback(
    () =>
      (
        document.querySelector<HTMLElement>(".lock-overlay button, .table-grid") ??
        editorRef.current
      )?.focus(),
    [],
  );
  const showTable = useCallback(
    (id: string | null, keepsFocus = false) => {
      const grid = gridRef.current;
      const host = hostRef.current;
      if (!grid || !host || id === tableRef.current) {
        return;
      }
      if (id === null) {
        host.inert = false;
        grid.resume();
        zoomSignal.set(grid.zoom);
      } else {
        actionsRef.current.commitPending();
        grid.pause();
      }
      tableRef.current = id;
      setTable(id === null ? null : { id, focus: !keepsFocus });
    },
    [zoomSignal],
  );
  const dialogOpener = useOpener(dialog !== null);
  const renameOpener = useOpener(renamingSheet !== null);
  const modalOpener = useOpener(modalMode !== null);
  const toolOpener = useOpener(m3aTool !== null);
  const cloudOpener = useOpener(collabMenuOpen);
  const historyOpener = useOpener(historyOpen);
  const aiOpener = useOpener(aiOpen);
  // Closing the panel only hides it: a review and a run in it go on.
  const aiShown = useRef(false);
  aiShown.current ||= aiOpen;

  const onSheet = useCallback<EditorHandle["onSheet"]>((sheet, then) => {
    const grid = gridRef.current;
    if (grid && sheet === activeSheetRef.current) {
      then(grid);
    } else {
      pendingJumpRef.current = { sheet, then };
      clientRef.current?.send({ kind: "switchSheet", sheet });
    }
  }, []);

  const handleJumpMatch = useCallback(
    ({ sheet, row, col }: Pick<WireSearchMatch, "sheet" | "row" | "col">) =>
      onSheet(sheet, (grid) => grid.jumpTo(row, col, false)),
    [onSheet],
  );

  /** Does what was to be done once this sheet is in view. */
  const arriveAt = useCallback((sheet: SheetInfo) => {
    if (askedSheetRef.current === sheet.sheet) {
      askedSheetRef.current = null;
    }
    const pending = pendingJumpRef.current;
    if (pending?.sheet === sheet.sheet && gridRef.current) {
      pendingJumpRef.current = null;
      pending.then(gridRef.current, sheet);
    }
  }, []);

  /** Continues what is in a part of some cells over the rest of them. */
  const fill = useCallback(
    (src: Span, dst: Span): boolean => {
      const filled = mutate(
        {
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
        },
        { action: "filledRange", computes: true },
      );
      if (filled) {
        gridRef.current?.selectRange(
          { row: dst.firstRow, col: dst.firstCol },
          { row: dst.endRow - 1, col: dst.endCol - 1 },
        );
      }
      return filled;
    },
    [mutate],
  );

  const autoFillDown = useCallback(
    (src: Span): boolean =>
      mutate(
        {
          kind: "autoFillDown",
          sheet: activeSheetRef.current,
          srcFirstRow: src.firstRow,
          srcRowCount: src.endRow - src.firstRow,
          srcFirstCol: src.firstCol,
          srcColCount: src.endCol - src.firstCol,
        },
        { action: "autoFilledColumn", computes: true },
      ),
    [mutate],
  );

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
    pendingRangeRefRevsRef.current.length = 0;
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
      const colId = sel.colId ?? grid.colIdAt(sel.col);
      if (rowId === null || colId === null) {
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
        editRev: 0,
      };
      editSessionRef.current = session;
      return session;
    },
    [],
  );

  const pendingAnalyzeSourcesRef = useRef<string[]>([]);
  const pendingRangeRefRevsRef = useRef<number[]>([]);

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

    /** A cell's name, with the row's number in the sheet itself. */
    const nameOfCell = ({ row, col }: { readonly row: number; readonly col: number }) =>
      `${columnName(col)}${grid.displayRowToPhysicalRow(row) + 1}`;

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
        lastSelectionRef.current = sel;
        setSelection(sel);
        requestInspect(sel);
        schedulePresenceSend();
      },
      onActivateEdit: () => {
        if (!effectiveReadOnlyRef.current) {
          editor.startEdit("doubleClick");
        }
      },
      onResizeRows: (sizes) =>
        mutate(
          {
            kind: "resizeRows",
            sheet: activeSheetRef.current,
            sizes: sizes.map((s) => ({ run: s.run, sizePx: s.sizePx ?? undefined })),
          },
          { action: "resizedRows" },
        ),
      onResizeCols: (sizes) =>
        mutate(
          {
            kind: "resizeCols",
            sheet: activeSheetRef.current,
            sizes: sizes.map((s) => ({ run: s.run, sizePx: s.sizePx ?? undefined })),
          },
          { action: "resizedColumns" },
        ),
      onMoveRows: (rows, afterRowId) =>
        mutate(
          { kind: "moveRows", sheet: activeSheetRef.current, rows: [...rows], afterRowId },
          { action: "movedRows" },
        ),
      onMoveCols: (cols, afterColId) =>
        mutate(
          { kind: "moveCols", sheet: activeSheetRef.current, cols: [...cols], afterColId },
          { action: "movedColumns" },
        ),
      onFillRange: fill,
      onAutoFillDown: autoFillDown,
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
        session.editRev += 1;
        pendingRangeRefRevsRef.current.push(session.editRev);
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
      onFilterButtonClick: (col) => {
        if (!isTouchMode()) {
          setM3aTool({ kind: "filter", col });
        }
      },
      zoom: true,
      onZoomChange: zoomSignal.set,
      onContextMenu: (target) => setMenu({ kind: "grid", target }),
    });
    gridRef.current = grid;

    const editCell = (sheet: number, { rowId, colId }: CellIds, raw: string) =>
      mutate({ kind: "editCell", sheet, rowId, colId, raw }, { computes: true });

    const held = new HeldCommits({
      grid,
      sheetId: () => activeSheetRef.current,
      inspect: (sheet, ids) => send({ kind: "inspectCell", sheet, ...ids }),
      commit: ({ sheet, raw }, ids) => editCell(sheet, ids, raw),
      fail: (one) => {
        const cell = nameOfCell(one);
        const text = one.raw;
        reject({
          say: () => t("The text for {cell} could not be entered: {text}", { cell, text }),
        });
      },
    });
    heldRef.current = held;

    const views = new ViewMemory(grid);
    /** Whether the sheets are those of a past version or a draft. */
    let isPreview = false;
    /** Opens a sheet where it was left. */
    const openSheet = (sheet: SheetInfo) => {
      grid.setSheet(sheet, views.open(activeDocIdRef.current, sheet.sheet, isPreview));
    };
    grid.watchFrames(() => views.touch());

    const editor = new CellEditor(grid, element, {
      isReadOnly: () => effectiveReadOnlyRef.current,
      isTouchOnly: isTouchMode,
      textOf: (cell) => {
        const cur = grid.selectionDetails();
        const known = inspectedRef.current;
        // What is shown of a formula is its value.
        return cur &&
          cur.row === cell.row &&
          cur.col === cell.col &&
          known &&
          known.sheet === activeSheetRef.current &&
          known.rowId === cur.rowId &&
          known.colId === cur.colId
          ? known.raw
          : null;
      },
      onCommit: (cell, raw, isRest) => {
        const session = editSessionRef.current;
        clearEditState(grid, editor);
        if (effectiveReadOnlyRef.current) {
          return;
        }
        if (!isRest && raw !== inspectedRef.current?.raw) {
          laterNow()?.announceCommit(nameOfCell(cell), raw);
        }
        const sheet = session?.hostSheet ?? activeSheetRef.current;
        if (session && !isRest) {
          editCell(sheet, session, raw);
        } else {
          held.hold({ sheet, row: cell.row, col: cell.col, raw, isRest }, session);
        }
        if (session && activeSheetRef.current !== session.hostSheet) {
          send({ kind: "switchSheet", sheet: session.hostSheet });
        }
      },
      onClear: () => {
        const runs = grid.selectedCellRuns();
        if (runs) {
          mutate(
            { kind: "clearCells", sheet: activeSheetRef.current, ...runs },
            { action: "clearedCells" },
          );
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
            session.editRev += 1;
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
        if (editSessionRef.current) {
          editSessionRef.current.editRev += 1;
        }
        clientRef.current?.send({ kind: "toggleFormulaDollar", source: text, cursorUtf16 });
      },
      onFormulaKeyDown: (event, text, cursorUtf16) => {
        return formulaBarRef.current?.handleAssistKeyDown(event, text, cursorUtf16) ?? false;
      },
      isPointAndClickActive,
      onJumpEdge: (dir, select) => {
        const cur = select ? grid.selectionFocus : grid.selection;
        if (cur) {
          send({
            kind: "jumpEdge",
            sheet: activeSheetRef.current,
            row: cur.row,
            col: cur.col,
            dir,
            select,
          });
        }
      },
      onJumpUsedEnd: (select) =>
        send({ kind: "jumpUsedEnd", sheet: activeSheetRef.current, select }),
      onUndo: () => mutate({ kind: "undo" }, { action: "undidAction" }),
      onRedo: () => mutate({ kind: "redo" }, { action: "redidAction" }),
      onCopy: (cut, clipboardData) => {
        const range = grid.selectionRange;
        if (!range || workerTerminatedRef.current) {
          return;
        }
        if (cut && effectiveReadOnlyRef.current) {
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
        send({
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
        const range = grid.selectionRange;
        if (!range) {
          return;
        }
        let effTsv = tsv;
        // Texts of data tables stay texts, whatever they look like.
        let effHtml = html?.replace(/(<td[^>]* data-reticula-text[^>]*>)(?!<\/td)/g, "$1'");
        if (effTsv.length === 0 && !effHtml && lastCopiedRef.current) {
          effTsv = lastCopiedRef.current.tsv;
          effHtml = lastCopiedRef.current.html;
        }
        if (effTsv.length === 0 && !effHtml) {
          return;
        }
        mutate(
          {
            kind: "pasteClipboard",
            sheet: activeSheetRef.current,
            destRow: range.firstRow,
            destRowCount: range.endRow - range.firstRow,
            destCol: range.firstCol,
            destColCount: range.endCol - range.firstCol,
            tsv: effTsv,
            html: effHtml,
          },
          { computes: true },
        );
      },
    });
    editorRef.current = editor;

    /** Leaves the workbook to another tab, with what cannot be saved any more kept in view. */
    const yieldDoc = (id: string, evicted: boolean) => {
      const session = editSessionRef.current;
      const { editing } = editor;
      const lost = session
        ? { cell: session.address, text: session.source }
        : editing && { cell: nameOfCell(editing), text: editor.text };
      clearEditState(grid, editor);
      setLockOverlay({
        kind: "yielded",
        docId: id,
        title: activeDocTitleRef.current,
        evicted,
        lost: lost && lost.text !== inspectedRef.current?.raw ? lost : null,
      });
    };

    const mountWorkerClient = (
      targetDocId: string,
      initialCmd: HostCommand,
      nextCollab: CollabWorkbookTarget | null = null,
    ) => {
      window.clearTimeout(presenceTimerRef.current);
      presenceTimerRef.current = 0;
      collabTransportRef.current?.dispose();
      collabTransportRef.current = null;
      clientRef.current?.stop();
      held.clear();
      workerTerminatedRef.current = false;
      activeDocIdRef.current = targetDocId;
      docLifecycleRef.current = initialCmd.kind === "takeOverDoc" ? "takingOver" : "starting";

      collabTargetRef.current = nextCollab;
      collabReplicaRef.current = null;
      collabPendingRef.current = 0;
      collabSlotRef.current = null;
      collabPresencesRef.current = [];
      const initialConn: CollabHookStatus["connection"] = nextCollab ? "connecting" : "offline";
      collabConnRef.current = initialConn;
      grid.setRemotePresences([]);
      setCollabTarget(nextCollab);
      setCollabConn(initialConn);
      setCollabPending(0);
      setCollabPresences([]);
      setCollabNotice(null);
      setHistoryOpen(false);
      setHistoryPreviewSeq(null);
      setHistorySeekMs(null);
      if (nextCollab) {
        activeDocTitleRef.current = nextCollab.title;
        setDocId(nextCollab.docId);
        setDocTitle(nextCollab.title);
      }

      const earlyCollabEvents: CollabEvent[] = [];
      let session: ((event: CollabEvent) => void) | null = null;
      let cancelled = false;

      const client = startEngine({
        tabId: tabIdRef.current,
        initialDocId: targetDocId,
        initialCommand: initialCmd,
        onCollab(event) {
          if (session) {
            session(event);
          } else {
            earlyCollabEvents.push(event);
          }
        },
        onMessage(message) {
          for (const intercept of interceptorsRef.current) {
            if (intercept(message)) {
              return;
            }
          }
          switch (message.kind) {
            case "collabOpened": {
              window.clearTimeout(yieldWatchdog);
              activeDocIdRef.current = message.docId;
              activeDocTitleRef.current = message.title;
              storageBackendRef.current = message.storageBackend;
              activeSlotRef.current = null;
              collabSlotRef.current = message.slot;
              collabReplicaRef.current = message.replica;
              waitingForFileRef.current = false;
              safeModeRef.current = null;
              setDocId(message.docId);
              setDocTitle(message.title);
              setLockOverlay(null);
              setCsvProgress(null);
              setModalMode(null);
              refreshStorageEstimate();
              break;
            }
            case "collabUnsyncedSaved": {
              setUnsyncedRescue({
                localDocId: message.localDocId,
                title: message.title,
              });
              break;
            }
            case "docOpened": {
              window.clearTimeout(yieldWatchdog);
              activeDocIdRef.current = message.docId;
              activeDocTitleRef.current = message.title;
              storageBackendRef.current = message.storageBackend;
              activeSlotRef.current = message.activeSlot;
              waitingForFileRef.current = false;
              safeModeRef.current = null;
              setDocId(message.docId);
              setOpening(message.opening);
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
              setCsvProgress(null);
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
              yieldDoc(message.docId, message.evicted);
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
              break;
            }
            case "quotaExceeded": {
              // The copies of tables, which can be had again, make room for what cannot.
              void laterNow()
                ?.dropCopies()
                .catch(() => {});
              setQuotaBanner(message.message);
              break;
            }
            case "csvImportProgress": {
              setCsvProgress(message);
              break;
            }
            case "fileExported": {
              const { format, filename, bytes } = message;
              Promise.all([
                import("./download.ts"),
                format === "xlsxSnapshot"
                  ? import("./xlsx/xlsxBridge.ts")
                      .then((m) => m.exportSnapshotBytesToXlsx(new Uint8Array(bytes)))
                      .then((res) => res.bytes)
                  : bytes,
              ])
                .then(([{ download }, file]) => download(filename, file))
                .catch((err) => reject(err instanceof Error ? err.message : String(err)));
              break;
            }
            case "closedForUpdate": {
              for (const resolve of closeWaiters.splice(0)) {
                resolve();
              }
              break;
            }
            case "historyReady": {
              activeSheetRef.current = message.sheet.sheet;
              lastInspectKey = "";
              inspectedRef.current = null;
              setInspected(null);
              isPreview = true;
              openSheet(message.sheet);
              setEngine((prev) =>
                prev.status === "ready" ? { ...prev, sheet: message.sheet } : prev,
              );
              setHistoryPreviewSeq(message.seq);
              setHistorySeekMs(message.elapsedMs);
              arriveAt(message.sheet);
              requestInspect(grid.selectionDetails(), true);
              break;
            }
            case "historyExited":
            case "draftReady":
            case "draftExited": {
              activeSheetRef.current = message.sheet.sheet;
              walSeqRef.current = message.sheet.walSeq;
              lastInspectKey = "";
              inspectedRef.current = null;
              setInspected(null);
              isPreview = message.kind === "draftReady";
              openSheet(message.sheet);
              setEngine((prev) =>
                prev.status === "ready" ? { ...prev, sheet: message.sheet } : prev,
              );
              if (message.kind === "historyExited") {
                setHistoryPreviewSeq(null);
                setHistorySeekMs(null);
              } else {
                setDraftPreview(message.kind === "draftReady");
                if (message.kind === "draftExited") {
                  grid.setDiffHighlights(null);
                }
              }
              requestInspect(grid.selectionDetails(), true);
              break;
            }
            case "ready":
              performance.mark(MARKS.engineReady);
              askedSheetRef.current = null;
              pendingJumpRef.current = null;
              // What has not been entered by now is not for this workbook.
              held.clear();
              docLifecycleRef.current = "ready";
              kernelVersionRef.current = message.info.kernelVersion;
              activeSheetRef.current = message.sheet.sheet;
              walSeqRef.current = message.sheet.walSeq;
              lastInspectKey = "";
              inspectedRef.current = null;
              setInspected(null);
              isPreview = false;
              // A draft is of the workbook that was open before.
              setDraftPreview(false);
              openSheet(message.sheet);
              setEngine({ status: "ready", info: message.info, sheet: message.sheet });
              // What was being typed before an update, which is rarely anything.
              if (hasKeptEdit()) {
                void loadLater().then(({ takeKeptEdit }) => {
                  const kept = takeKeptEdit(editDocKey());
                  if (kept) {
                    // Once it can be typed into again.
                    window.setTimeout(() => actionsRef.current.restoreEdit(kept), 0);
                  }
                });
              }
              break;
            case "sheetChanged": {
              const switched = message.sheet.sheet !== activeSheetRef.current;
              activeSheetRef.current = message.sheet.sheet;
              walSeqRef.current = message.sheet.walSeq;
              if (switched) {
                lastInspectKey = "";
                inspectedRef.current = null;
                setInspected(null);
                openSheet(message.sheet);
                grid.setActiveSpillRange(null);
                if (collabTargetRef.current) {
                  sendPresenceNow(message.sheet.sheet);
                }
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
              grid.setFilterState(message.sheet.filter ?? null, message.sheet.totalRows);
              arriveAt(message.sheet);
              syncGridFormulaRefs(grid, analysisRef.current, message.sheet.sheet);
              setEngine((prev) =>
                prev.status === "ready" ? { ...prev, sheet: message.sheet } : prev,
              );
              const action = pendingActionRef.current;
              pendingActionRef.current = "updated";
              setActivity({
                kind: "done",
                action,
                ms: message.actionMs,
                recalculated: message.recalculated,
              });
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
              held.retry(message);
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
                ...message,
                error: message.error ?? null,
                spillAnchor: message.spillAnchor ?? null,
              };
              inspectedRef.current = info;
              setInspected(info);
              editor.supplyText();
              grid.setActiveSpillRange(message.spillRange ?? null);
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
              session.editRev += 1;
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
              const reqRev = pendingRangeRefRevsRef.current.shift();
              const session = editSessionRef.current;
              if (!session || (reqRev !== undefined && reqRev !== session.editRev)) {
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
              lastCopiedRef.current = message;
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
              if (navigator.clipboard) {
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
              setActivity({
                kind: "done",
                action: "pasted",
                ms: message.elapsedMs,
                recalculated: message.recalculated,
              });
              requestInspect(grid.selectionDetails(), true);
              refreshStorageEstimate();
              break;
            }
            case "recalcProgress":
              setActivity({ kind: "recalculating", done: message.done, total: message.total });
              break;
            case "recalcDone":
              if (pendingActionRef.current === "clearedCells") {
                pendingActionRef.current = "updated";
                laterNow()?.announceCleared(
                  grid.selectionDetails(),
                  (row) => grid.displayRowToPhysicalRow(row) + 1,
                );
              }
              setRejectedReason(null);
              walSeqRef.current = message.sheet.walSeq;
              grid.updateSheet(message.sheet);
              setEngine((prev) =>
                prev.status === "ready" ? { ...prev, sheet: message.sheet } : prev,
              );
              setActivity(
                message.recalculated > 0
                  ? { kind: "recalculated", cells: message.recalculated, ms: message.elapsedMs }
                  : { kind: "done", action: "updatedCell", ms: message.elapsedMs, recalculated: 0 },
              );
              requestInspect(grid.selectionDetails(), true);
              refreshStorageEstimate();
              break;
            case "filterValues":
            case "searchResults":
            case "namedRangesListed":
              setM3aEvent(message);
              break;
            case "searchNavigated":
              setM3aEvent(message);
              if (message.found) {
                handleJumpMatch(message.found);
              }
              break;
            case "replaceCompleted":
              walSeqRef.current = message.sheet.walSeq;
              grid.updateSheet(message.sheet);
              grid.setFilterState(message.sheet.filter ?? null, message.sheet.totalRows);
              setEngine((prev) =>
                prev.status === "ready" ? { ...prev, sheet: message.sheet } : prev,
              );
              setM3aEvent(message);
              setActivity({ kind: "replaced", cells: message.replacedCells });
              requestInspect(grid.selectionDetails(), true);
              refreshStorageEstimate();
              break;
            case "rejected":
              // What is said of the next change, perhaps by others, is not what was asked for here.
              pendingActionRef.current = "updated";
              setCsvProgress(null);
              askedSheetRef.current = null;
              reject(message.reason);
              if (collabTargetRef.current) {
                setCollabNotice(message.reason);
              }
              break;
            case "failed":
              docLifecycleRef.current = "failed";
              setEngine({ status: "failed", message: message.message });
              announce(t("The engine has stopped. Reload the page to start again."), {
                politeness: "assertive",
              });
              break;
          }
        },
        onPage: (page) => {
          grid.acceptPage(page);
          held.retry();
        },
      });

      clientRef.current = client;

      if (nextCollab) {
        void import("./collab/session.ts").then(({ startCollabSession }) => {
          if (cancelled || clientRef.current !== client) {
            return;
          }
          const started = startCollabSession({
            target: nextCollab,
            client,
            grid,
            editor,
            activeSheet: activeSheetRef,
            selection: lastSelectionRef,
            kernelVersion: kernelVersionRef,
            connection: collabConnRef,
            replica: collabReplicaRef,
            pending: collabPendingRef,
            presences: collabPresencesRef,
            transport: collabTransportRef,
            manualOffline: collabManualOfflineRef,
            onConnection: setCollabConn,
            onPending: setCollabPending,
            onPresences: setCollabPresences,
            onNotice: setCollabNotice,
            onCheckUpdate: () => void swClientRef.current?.checkForUpdate(),
            sendPresence: sendPresenceNow,
          });
          session = started;
          earlyCollabEvents.splice(0).forEach(started);
        });
      }

      const prevStop = client.stop;
      client.stop = () => {
        cancelled = true;
        prevStop();
      };
    };

    startWorkerFnRef.current = mountWorkerClient;
    if (!initialRoute.current.collabId && !initialRoute.current.joinToken) {
      mountWorkerClient(initialRoute.current.docId, {
        kind: "openDoc",
        docId: initialRoute.current.docId,
        safeRequested: initialRoute.current.safeRequested,
      });
    }
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
            yieldDoc(activeDocIdRef.current, true);
          }, YIELD_WATCHDOG_MS);
        }
      };
    }

    const flushAndCloseDoc = async (): Promise<void> => {
      const tablesClosed = laterNow()?.closeTables();
      if (workerTerminatedRef.current || !clientRef.current) {
        return tablesClosed;
      }
      clientRef.current.sendHost({ kind: "flushAndCloseForUpdate" });
      await new Promise<void>((resolve) => {
        const cap = window.setTimeout(resolve, 1_500);
        closeWaiters.push(() => {
          window.clearTimeout(cap);
          resolve();
        });
      });
      await tablesClosed;
    };

    let swStartTimer = 0;
    let swClient: Promise<SwClientHandle> | null = null;
    let disposed = false;
    const ensureSwClient = (): Promise<SwClientHandle> => {
      swClient ??= loadLater().then(({ createSwClient }) => {
        const handle = createSwClient({
          tabId: tabIdRef.current,
          onUpdateReady: (ready) => setSwUpdateReady(ready),
          onDrainForUpdate: () => {
            // An update reloads the tab: what is being typed comes back afterwards.
            const session = editSessionRef.current;
            if (session) {
              laterNow()?.keepEdit({
                doc: editDocKey(),
                sheet: session.hostSheet,
                row: session.row,
                col: session.col,
                source: session.source,
                cursorUtf16: session.cursorUtf16,
                inFormulaBar: document.activeElement?.closest(".formulabar") != null,
              });
            }
            return flushAndCloseDoc();
          },
          onBusy: (onAgreed) => askTables({ kind: "giveUp", onAgreed }),
        });
        if (disposed) {
          handle.dispose();
        } else {
          swClientRef.current = handle;
        }
        return handle;
      });
      return swClient;
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
        if (!swClient && swStartTimer === 0) {
          grid.setRenderer(rendererPreference());
          // Defer Service Worker registration until after first-screen paint so
          // cold-start B10 latency is completely unaffected (`14-m1b.md` 7.4).
          swStartTimer = window.setTimeout(() => {
            swStartTimer = 0;
            // Without it the app works as it does in a browser that has no
            // Service Workers.
            ensureSwClient().catch(() => {});
            loadLaterWhenIdle(() => setIsLaterReady(true));
            grid.prepareTouch();
            loadPopup().catch(() => {});
          }, 0);
        }
        if (firstScreenMarked) {
          stopMarking();
        }
      }
    });

    internalsRef.current = { flushAndClose: flushAndCloseDoc, swClient: ensureSwClient };
    const timer = window.setInterval(() => setStats(grid.stats()), STATUS_INTERVAL_MS);

    // Release the dedicated worker (and thus its Web Lock and OPFS
    // SyncAccessHandle) on navigation or BFCache freeze, and transparently
    // reopen if restored from BFCache.
    const onPageHide = () => {
      views.save();
      window.clearTimeout(yieldWatchdog);
      window.clearTimeout(presenceTimerRef.current);
      presenceTimerRef.current = 0;
      collabTransportRef.current?.dispose();
      collabTransportRef.current = null;
      clientRef.current?.stop();
      workerTerminatedRef.current = true;
    };
    const onPageShow = (event: PageTransitionEvent) => {
      if (event.persisted && workerTerminatedRef.current) {
        const currentCollab = collabTargetRef.current;
        if (currentCollab) {
          openCollab(currentCollab);
        } else {
          mountWorkerClient(activeDocIdRef.current, {
            kind: "openDoc",
            docId: activeDocIdRef.current,
          });
        }
      }
    };
    window.addEventListener("pagehide", onPageHide);
    window.addEventListener("pageshow", onPageShow);

    return () => {
      views.save();
      window.removeEventListener("pagehide", onPageHide);
      window.removeEventListener("pageshow", onPageShow);
      window.clearInterval(timer);
      window.clearTimeout(yieldWatchdog);
      window.clearTimeout(swStartTimer);
      window.clearTimeout(presenceTimerRef.current);
      locksWatch?.close();
      collabTransportRef.current?.dispose();
      collabTransportRef.current = null;
      disposed = true;
      swClientRef.current?.dispose();
      swClientRef.current = null;
      stopMarking();
      editor.destroy();
      grid.destroy();
      clientRef.current?.stop();
      editorRef.current = null;
      gridRef.current = null;
      clientRef.current = null;
      startWorkerFnRef.current = null;
    };
  }, [
    arriveAt,
    askTables,
    autoFillDown,
    clearEditState,
    ensureEditSession,
    fill,
    handleJumpMatch,
    mutate,
    openCollab,
    refreshStorageEstimate,
    reject,
    requestAnalyze,
    schedulePresenceSend,
    send,
    sendPresenceNow,
    syncGridFormulaRefs,
    zoomSignal,
    editDocKey,
  ]);

  useGridStrings(gridRef);

  const wasReadOnly = useRef(effectiveReadOnly);
  useEffect(() => {
    editorRef.current?.refreshAttributes();
    if (effectiveReadOnly && !wasReadOnly.current) {
      const grid = gridRef.current;
      const editor = editorRef.current;
      if (grid && editor) {
        clearEditState(grid, editor);
      }
      // Whoever is in a drawer or a toolbar stays there.
      const focused = document.activeElement ?? document.body;
      editor?.cancelExternal(
        focused === document.body || focused.closest(".formulabar, .reticula-grid") !== null,
      );
      setRenamingSheet(null);
    }
    wasReadOnly.current = effectiveReadOnly;
  }, [effectiveReadOnly, clearEditState]);

  // What had the focus is gone with the lock.
  const wasLocked = useRef(false);
  useEffect(() => {
    if (wasLocked.current && !lockOverlay && document.activeElement === document.body) {
      editorRef.current?.focus();
    }
    wasLocked.current = lockOverlay !== null;
  }, [lockOverlay]);

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
    if (
      !grid ||
      !editor ||
      !client ||
      workerTerminatedRef.current ||
      effectiveReadOnlyRef.current
    ) {
      return;
    }
    const session = ensureEditSession(grid, next, cursorUtf16);
    if (!session) {
      return;
    }
    if (session.source !== next) {
      session.pointPickedSpan = null;
      session.editRev += 1;
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
    if (
      !grid ||
      !editor ||
      !client ||
      workerTerminatedRef.current ||
      effectiveReadOnlyRef.current
    ) {
      return;
    }
    const session = editSessionRef.current;
    if (session && activeSheetRef.current !== session.hostSheet) {
      const { hostSheet, rowId, colId, source } = session;
      clearEditState(grid, editor);
      mutate({ kind: "editCell", sheet: hostSheet, rowId, colId, raw: source }, { computes: true });
      send({ kind: "switchSheet", sheet: hostSheet });
      editor.focus();
      return;
    }
    pendingRangeRefRevsRef.current.length = 0;
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
    if (
      !grid ||
      !editor ||
      !client ||
      workerTerminatedRef.current ||
      effectiveReadOnlyRef.current
    ) {
      return;
    }
    const start = Math.max(0, Math.min(currentSource.length, item.replaceStartUtf16));
    const end = Math.max(start, Math.min(currentSource.length, item.replaceEndUtf16));
    const nextSource = currentSource.slice(0, start) + item.insertText + currentSource.slice(end);
    const nextCursor = start + item.insertText.length;

    const session = ensureEditSession(grid, nextSource, nextCursor);
    if (session) {
      session.editRev += 1;
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

  /** Shows another sheet. Unless the keys have the focus on its tab, the grid has it then. */
  const handleSwitchSheet = (targetId: number, keepsFocus: boolean) => {
    const editor = editorRef.current;
    const client = clientRef.current;
    if (!editor || !client || workerTerminatedRef.current) {
      return;
    }
    showTable(null);
    if (targetId === (askedSheetRef.current ?? activeSheetRef.current)) {
      if (!keepsFocus) {
        editor.focus();
      }
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
    commitPending();
    pendingActionRef.current = "switchedSheet";
    askedSheetRef.current = targetId;
    client.send({ kind: "switchSheet", sheet: targetId });
    if (!keepsFocus) {
      editor.focus();
    }
  };

  /** Accepts what is being typed, and leaves the focus where it is. */
  const commitPending = () => {
    if (editorRef.current?.editing || editSessionRef.current) {
      const { activeElement } = document;
      handleFormulaBarCommit(false);
      if (activeElement instanceof HTMLElement) {
        activeElement.focus({ preventScroll: true });
      }
    }
  };

  const handleTakeOver = (docId: string) => openWith(docId, { kind: "takeOverDoc", docId });

  const handleCreateNewWorkbook = (title: string, template: "empty" | "sample") => {
    const docId = makeFreshDocId();
    notePersistenceMutation();
    setModalMode(null);
    setLockOverlay(null);
    openWith(docId, { kind: "createDoc", docId, title, template });
  };

  const handleOpenWorkbooksDrawer = (importing = false) => {
    if (!workerTerminatedRef.current) {
      clientRef.current?.sendHost({ kind: "listDocs" });
    }
    refreshStorageEstimate();
    setModalMode({ kind: "workbooks", importing });
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

  const handleRenameSheet = (tab: SheetTabInfo, name: string | null, byKey: boolean) => {
    setRenamingSheet(null);
    if (name !== null) {
      mutate({ kind: "renameSheet", sheet: tab.id, name }, { action: "renamedSheet" });
    }
    if (byKey) {
      const opener = renameOpener.current;
      if (opener instanceof HTMLElement && opener.closest(".sheetbar")) {
        opener.focus();
      } else {
        focusGrid();
      }
    }
  };

  /** Does what a banner is for, which takes the banner away, and the focus with it. */
  const leaveBanner = (act: () => void) => {
    act();
    focusGrid();
  };

  const readySheet = engine.status === "ready" && lockOverlay === null ? engine.sheet : null;
  const state: EditorState = {
    sheet: readySheet,
    readOnly: effectiveReadOnly,
    selection,
    style: matchingInspected?.style,
    formatCode: matchingInspected?.formatCode ?? "General",
    isCloud: collabTarget !== null,
    table: table && { id: table.id, view: tableView },
  };
  const stateRef = useRef(state);
  stateRef.current = state;
  /**
   * What was being typed before an update, until its cell is known: a tab that
   * was reloaded behind another one has no pages yet, and nothing can be typed
   * into a cell that is not there.
   */
  const keptEditRef = useRef<KeptEdit | null>(null);
  /** And once it has been typed again into the formula bar, which then has the focus. */
  const keptForBarRef = useRef<KeptEdit | null>(null);

  const restoreEdit = (kept: KeptEdit) =>
    onSheet(kept.sheet, (grid) => {
      keptEditRef.current = kept;
      grid.select({ row: kept.row, col: kept.col });
    });

  /** Types it again, if its cell is the one selected and is there. */
  const typeKeptEdit = (selected: Selection | null) => {
    const kept = keptEditRef.current;
    const editor = editorRef.current;
    if (!kept || !editor || selected === null) {
      return;
    }
    const isThere =
      selected.row === kept.row &&
      selected.col === kept.col &&
      activeSheetRef.current === kept.sheet;
    if (isThere && selected.rowId === null) {
      return; // Until its page has come.
    }
    keptEditRef.current = null;
    // Another cell has been gone to meanwhile: it is for that one now.
    if (!isThere) {
      return;
    }
    if (effectiveReadOnlyRef.current || editSessionRef.current) {
      return;
    }
    if (kept.inFormulaBar) {
      keptForBarRef.current = kept;
      handleFormulaBarChange(kept.source, kept.cursorUtf16);
    } else {
      editor.startEdit("F2", kept.source);
      editor.applyTextAndSelection(kept.source, kept.cursorUtf16);
    }
  };
  useEffect(() => actionsRef.current.typeKeptEdit(selection), [selection]);
  useEffect(() => {
    const kept = keptForBarRef.current;
    if (kept && editingText === kept.source) {
      keptForBarRef.current = null;
      formulaBarRef.current?.applyTextAndSelection(kept.source, kept.cursorUtf16);
      formulaBarRef.current?.focus();
    }
  }, [editingText]);

  const actions = {
    tables,
    restoreEdit,
    typeKeptEdit,
    commitPending,
    handleSwitchSheet,
    handleCreateNewWorkbook,
    handleOpenWorkbooksDrawer,
  };
  const actionsRef = useRef(actions);
  actionsRef.current = actions;
  const [handle] = useState(
    (): EditorHandle => ({
      state: () => stateRef.current,
      sheetId: () => activeSheetRef.current,
      askedSheetId: () => askedSheetRef.current ?? activeSheetRef.current,
      grid: () => gridRef.current,
      cellEditor: () => editorRef.current,
      mutate,
      send,
      sendHost(command, transfer) {
        // Another workbook is to be opened: what is changed until then would be lost.
        if (/^import/.test(command.kind)) {
          setCsvProgress({ rowsParsed: 0, percent: 0 });
        }
        clientRef.current?.sendHost(command, transfer);
      },
      openWith,
      openCollab,
      collab: () => collabTargetRef.current,
      updateCollab(change) {
        setCollabTarget((prev) => {
          const next = prev && { ...prev, ...change };
          collabTargetRef.current = next;
          return next;
        });
      },
      presences: () => collabPresencesRef.current,
      noteChange: notePersistenceMutation,
      createWorkbook: (title, template) =>
        actionsRef.current.handleCreateNewWorkbook(title, template),
      freshDocId: makeFreshDocId,
      fill,
      autoFillDown,
      intercept(listener) {
        interceptorsRef.current.add(listener);
        return () => interceptorsRef.current.delete(listener);
      },
      commitPending: () => actionsRef.current.commitPending(),
      copied: () => lastCopiedRef.current,
      switchSheet: (sheet) => actionsRef.current.handleSwitchSheet(sheet, true),
      onSheet,
      newWorkbook: () => actionsRef.current.handleCreateNewWorkbook("Untitled workbook", "empty"),
      openTool: setM3aTool,
      openWorkbooks: (importing) => actionsRef.current.handleOpenWorkbooksDrawer(importing),
      openCloud: () => setCollabMenuOpen(true),
      toggleHistory: () => setHistoryOpen((open) => !open),
      toggleAi: () => setAiOpen((open) => !open),
      openDialog: setDialog,
      openMenu: setMenu,
      renameSheet: (tab) => setRenamingSheet(tab.id),
      editNameBox: () => setNameBoxEditing(true),
      docId: () => activeDocIdRef.current,
      tables: () => actionsRef.current.tables,
      showTable,
      askTables,
      notify: setQuotaBanner,
      reject,
    }),
  );
  // For tests and measurements, which ask for them in the page's address.
  useEffect(() => {
    if (!hooksRequested()) {
      return;
    }
    let remove = () => {};
    let isGone = false;
    void import("./hooks-install.ts").then(({ installHooks }) => {
      if (!isGone) {
        remove = installHooks(handle, {
          internals: internalsRef,
          docId: activeDocIdRef,
          title: activeDocTitleRef,
          engineStatus: docLifecycleRef,
          storageBackend: storageBackendRef,
          activeSlot: activeSlotRef,
          walSeq: walSeqRef,
          waitingForFile: waitingForFileRef,
          safeMode: safeModeRef,
          connection: collabConnRef,
          replica: collabReplicaRef,
          pending: collabPendingRef,
          slot: collabSlotRef,
          manualOffline: collabManualOfflineRef,
          transport: collabTransportRef,
        });
      }
    });
    return () => {
      isGone = true;
      remove();
    };
  }, [handle]);

  const onCommand = useCallback(
    (id: string, sheet?: number) => runCommand(handle, id, sheet),
    [handle],
  );

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const isEditing = editorRef.current?.editing != null;
      const id = commandFor(
        event,
        focusScope(
          event.target,
          document.querySelector(".table-grid") ?? hostRef.current,
          isEditing,
        ),
      );
      if (id !== null) {
        event.preventDefault();
        if (id === "contextMenu") {
          swallowContextMenu();
        }
        runCommand(handle, id);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [handle]);

  /** Takes files before an editor of text does. */
  const onDrag = (event: DragEvent<HTMLElement>) => {
    const { currentTarget, dataTransfer, type } = event;
    if (dataTransfer.types.includes("Files")) {
      event.preventDefault();
      event.stopPropagation();
      const files = [...dataTransfer.files];
      void loadLater().then(({ dragFiles }) => dragFiles(handle, currentTarget, type, files));
    }
  };

  const canEdit = !effectiveReadOnly && !table;
  const canDeleteRows = canEdit && selection !== null && selection.rowId !== null;
  const canDeleteCols = canEdit && selection !== null && selection.colId !== null;

  const nameBoxProps = {
    handle,
    isEditing: nameBoxEditing && readySheet !== null,
    onEdit: () => onCommand("goTo"),
    onDone: (byKey: boolean) => {
      setNameBoxEditing(false);
      if (byKey) {
        focusGrid();
      }
    },
  };
  const nameBox = table ? null : (
    <NameBox {...nameBoxProps} text={nameBoxText} inactive={readySheet === null} />
  );

  return (
    <div
      className="app"
      data-touch={touchMode || undefined}
      data-table={table ? "" : undefined}
      onDragOverCapture={onDrag}
      onDropCapture={onDrag}
    >
      <TitleBar
        handle={handle}
        title={docTitle}
        sheet={readySheet}
        isTable={table !== null}
        collabRole={collabTarget?.role ?? null}
        readOnly={!canEdit}
        canDeleteRows={canDeleteRows}
        storageUsedMb={storageUsedMb}
        aiOpen={aiOpen}
        historyOpen={historyOpen}
        onNavigate={onNavigate}
        onCommand={onCommand}
      />

      {(collabMenuOpen ||
        collabTarget !== null ||
        initialRoute.current.collabId !== null ||
        initialRoute.current.joinToken !== null ||
        initialRoute.current.shareToken !== null ||
        unsyncedRescue !== null ||
        collabNotice !== null) && (
        <Suspense fallback={null}>
          <LazyCollabHost
            handle={handle}
            localDocId={DEFAULT_DOC_ID}
            open={collabMenuOpen}
            initialCollabId={initialRoute.current.collabId}
            initialJoinToken={initialRoute.current.joinToken}
            initialShareToken={initialRoute.current.shareToken}
            activeTarget={collabTarget}
            readOnly={effectiveReadOnly}
            historyOpen={historyOpen}
            modalOpener={cloudOpener}
            historyOpener={historyOpener}
            onFocusGrid={focusGrid}
            historyPreviewSeq={historyPreviewSeq}
            historyPreviewElapsedMs={historySeekMs}
            connection={collabConn}
            pendingCount={collabPending}
            remotePresences={collabPresences}
            notice={collabNotice}
            unsyncedRescue={unsyncedRescue}
            onRescued={() => setUnsyncedRescue(null)}
            onCloseModal={() => setCollabMenuOpen(false)}
            onRequestOpenModal={() => setCollabMenuOpen(true)}
            onCloseHistory={() => setHistoryOpen(false)}
            onNavigate={onNavigate}
          />
        </Suspense>
      )}

      <FormatToolbar
        sheet={readySheet}
        readOnly={effectiveReadOnly}
        canFormat={readySheet !== null && selection !== null && !effectiveReadOnly}
        canDeleteCols={canDeleteCols}
        style={state.style}
        formatCode={state.formatCode}
        onCommand={onCommand}
      />

      <div className="banners">
        {(quotaBanner !== null ||
          swUpdateReady ||
          readySheet?.recalcFrozen === true ||
          draftPreview ||
          historyPreviewSeq !== null ||
          csvProgress !== null ||
          rejectedReason !== null) && (
          <Suspense fallback={null}>
            <LazyBanners
              sheet={readySheet}
              importing={csvProgress}
              rejected={rejectedReason}
              onDismissRejected={() => leaveBanner(() => setRejectedReason(null))}
              storage={quotaBanner}
              onDismissStorage={() => leaveBanner(() => setQuotaBanner(null))}
              onExportRtcl={() => onCommand("exportRtcl")}
              onOpenWorkbooks={() => onCommand("workbooks")}
              updateReady={swUpdateReady}
              onUpdate={() => swClientRef.current?.applyUpdate()}
              onDismissUpdate={() => leaveBanner(() => setSwUpdateReady(false))}
              onResumeRecalc={() => leaveBanner(() => onCommand("resumeRecalc"))}
              isDraft={draftPreview}
              onExitDraft={() =>
                leaveBanner(() => clientRef.current?.sendHost({ kind: "draftExit" }))
              }
              historySeq={historyPreviewSeq}
              onReturnLive={() =>
                leaveBanner(() => clientRef.current?.sendHost({ kind: "historyExit" }))
              }
            />
          </Suspense>
        )}
        <DemoTip sheet={readySheet} onJump={handleJumpToV1} />
      </div>

      {!touchMode && (
        <FormulaBar
          ref={formulaBarRef}
          nameBox={nameBox}
          inert={lockOverlay !== null || table !== null}
          value={formulaBarValue}
          placeholder={
            effectiveReadOnly
              ? t("Read-only mode")
              : selection || editSessionRef.current
                ? t("Enter a value or =formula (Tab to autocomplete, F4 to toggle $)")
                : t("Select a cell to edit")
          }
          disabled={
            effectiveReadOnly ||
            readySheet === null ||
            ((selection === null || selection.rowId === null) && !editSessionRef.current)
          }
          isEditing={editingText !== null}
          inspected={matchingInspected !== null}
          isSpillShadow={matchingInspected?.isSpillShadow === true}
          analysis={formulaAnalysis?.analysis ?? null}
          analysisSource={formulaAnalysis?.source ?? null}
          getEditSource={getEditSource}
          onChange={handleFormulaBarChange}
          onCommit={handleFormulaBarCommit}
          onCancel={handleFormulaBarCancel}
          onToggleDollar={(text, cursorUtf16) => {
            if (effectiveReadOnlyRef.current) {
              return;
            }
            if (editSessionRef.current) {
              editSessionRef.current.editRev += 1;
            }
            clientRef.current?.send({ kind: "toggleFormulaDollar", source: text, cursorUtf16 });
          }}
          onAcceptCompletion={handleAcceptCompletion}
        />
      )}

      {aiShown.current && (
        <Suspense fallback={null}>
          <LazyAiHost
            handle={handle}
            open={aiOpen}
            opener={aiOpener}
            onFocusGrid={focusGrid}
            activeSheetId={engine.status === "ready" ? engine.sheet.sheet : 0}
            draftActive={draftPreview}
            onClose={() => setAiOpen(false)}
          />
        </Suspense>
      )}

      <main className="sheet">
        <div
          ref={hostRef}
          className="grid"
          data-testid="grid"
          inert={lockOverlay !== null || table !== null}
        />
        {isLaterReady && (
          <Suspense fallback={null}>
            <LazyTableDock
              key={`${docId} ${opening}`}
              handle={handle}
              docId={docId}
              tabId={tabIdRef.current}
              shown={table}
              view={tableView}
              ask={tableAsk}
              isCloud={collabTarget !== null}
              isOnline={collabConn === "online"}
              isClosed={readySheet === null || historyPreviewSeq !== null || draftPreview}
              readOnly={effectiveReadOnly}
              nameBox={nameBoxProps}
              zoom={zoomSignal}
              onTables={setTables}
              onView={setTableView}
              onAsked={() => setTableAsk(null)}
            />
          </Suspense>
        )}
        {lockOverlay && (
          <Suspense fallback={null}>
            <LazyLockOverlay
              lock={lockOverlay}
              onTakeOver={handleTakeOver}
              onNewWorkbook={handle.newWorkbook}
            />
          </Suspense>
        )}
        {engine.status === "failed" && (
          <div className="failure">
            <p>{t("The engine has stopped. Reload the page to start again.")}</p>
            <p className="detail" data-testid="engine-error">
              {engineText(engine.message)}
            </p>
          </div>
        )}
      </main>

      {touchMode && (
        <Suspense fallback={null}>
          <LazyTouchBar
            nameBox={nameBox}
            inspection={matchingInspected}
            text={selection?.text ?? ""}
            inert={lockOverlay !== null || table !== null}
          />
        </Suspense>
      )}

      {readySheet && (
        <SheetTabs
          sheet={readySheet}
          tables={tables}
          table={table?.id ?? null}
          readOnly={effectiveReadOnly}
          renaming={renamingSheet}
          onRename={handleRenameSheet}
          onSwitch={handleSwitchSheet}
          onMenu={(tab, clientX, clientY) => setMenu({ kind: "tab", clientX, clientY, tab })}
          onCommand={onCommand}
          onTable={showTable}
          onTables={askTables}
        />
      )}

      <StatusBar
        status={
          lockOverlay
            ? lockOverlay.kind
            : modalMode?.kind === "safeMode"
              ? "safeMode"
              : engine.status
        }
        opening={engine.status === "starting" && !lockOverlay && !modalMode}
        sheet={readySheet}
        selection={selection}
        activity={(activity && laterNow()?.activityText(activity)) ?? null}
        stats={stats}
        kernelVersion={engine.status === "ready" ? engine.info.kernelVersion : ""}
        zoom={{
          zoom: zoomSignal,
          handle,
          onStep: (direction) => onCommand(direction > 0 ? "zoomIn" : "zoomOut"),
        }}
      />

      {isLaterReady && (
        <Suspense fallback={null}>
          <LazyAnnouncements
            host={hostRef}
            documentId={docId}
            sheet={table ? null : readySheet}
            selection={selection}
            inspection={matchingInspected}
            activity={activity}
            rowNumber={(row) => (gridRef.current?.displayRowToPhysicalRow(row) ?? row) + 1}
          />
        </Suspense>
      )}
      {menu && (
        <Suspense fallback={null}>
          <LazyMenus handle={handle} menu={menu} onClose={() => setMenu(null)} />
        </Suspense>
      )}

      {dialog && (
        <Suspense fallback={null}>
          <LazyDialogs
            handle={handle}
            dialog={dialog}
            opener={dialogOpener}
            onFocusGrid={focusGrid}
            onClose={() => setDialog(null)}
          />
        </Suspense>
      )}

      {modalMode && (
        <Suspense fallback={null}>
          <LazyLocalModals
            handle={handle}
            mode={modalMode}
            opener={modalOpener}
            onFocusGrid={focusGrid}
            activeDocId={docId}
            docs={catalogDocs}
            storageUsedMb={storageUsedMb}
            onClose={() => setModalMode(null)}
            onNotice={setQuotaBanner}
            onRejected={reject}
          />
        </Suspense>
      )}

      {m3aTool && readySheet && (
        <Suspense fallback={null}>
          <LazyM3aToolsModal
            mode={m3aTool}
            opener={toolOpener}
            onFocusGrid={focusGrid}
            sheet={readySheet}
            selection={selection}
            readOnly={effectiveReadOnly}
            lastEvent={m3aEvent}
            onSend={handleM3aSend}
            onMutate={notePersistenceMutation}
            onJumpMatch={handleJumpMatch}
            onClose={handleCloseM3aTool}
          />
        </Suspense>
      )}
    </div>
  );
}
