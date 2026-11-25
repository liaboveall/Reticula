import {
  Component,
  lazy,
  type ReactNode,
  Suspense,
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";
import type { EditorHandle, ShownTable, TableAsk, TableView } from "../commands/handle.ts";
import { t } from "../i18n/index.ts";
import { hasLeftovers, readCatalog, type TableEntry } from "../tables/catalog.ts";
import type { UploadProgress } from "../tables/uploads.ts";
import type { NameBoxProps } from "./NameBox.tsx";
import { RemoteTables, resumeQueued } from "./remote-tables.ts";
import { TableViews } from "./table-views.ts";
import type { ZoomSignal } from "./ZoomControl.tsx";

/** Loads what shows tables, and uploads them. */
export const loadTables = () => import("../tables/ui/Tables.tsx");
/** Where loading it fails, that is remembered: another one tries again. */
const lazyTables = () => lazy(loadTables);

const missingText = () =>
  t("What shows data tables could not be loaded. Check the connection, and try again.");

export interface TableDockProps {
  readonly handle: EditorHandle;
  readonly docId: string;
  readonly tabId: string;
  /** The data table in view. */
  readonly shown: ShownTable | null;
  readonly view: TableView | null;
  readonly ask: TableAsk | null;
  /** Whether the workbook, with its data tables, is on the server. */
  readonly isCloud: boolean;
  /** Whether the server can be had. */
  readonly isOnline: boolean;
  /** Whether there is no worksheet to come back to: it is locked, or of a past version. */
  readonly isClosed: boolean;
  readonly readOnly: boolean;
  readonly nameBox: Omit<NameBoxProps, "text" | "inactive">;
  readonly zoom: ZoomSignal;
  readonly onTables: (tables: readonly TableEntry[]) => void;
  readonly onView: (view: TableView | null) => void;
  /** What was asked for is being done. */
  readonly onAsked: () => void;
}

/** What the address may ask for: the built-in table, with so many rows. */
export const SAMPLE_PARAM = "sample";

/** Rows that a table can have. */
const MOST_ROWS = 100_000_000;

/** How many rows of the built-in table the address asks for, if it does. Once. */
function askedSample(): number | null {
  const url = new URL(window.location.href);
  const rows = url.searchParams.get(SAMPLE_PARAM);
  if (rows === null) {
    return null;
  }
  url.searchParams.delete(SAMPLE_PARAM);
  window.history.replaceState(window.history.state, "", url);
  const asked = Number(rows);
  return Number.isInteger(asked) && asked > 0 && asked <= MOST_ROWS ? asked : null;
}

/** Why files that are dropped are not taken, if they are not. */
function refusal(files: readonly File[], readOnly: boolean): string | null {
  if (document.querySelector("dialog:modal")) {
    return t("Close the dialog before you drop a file here.");
  }
  if (readOnly) {
    return t("Nothing can be imported while the workbook is read-only.");
  }
  return files.length === 1 ? null : t("Drop one file at a time.");
}

let dropTimer = 0;

/** Files are dragged over the editor, or dropped. */
export function dragFiles(
  handle: EditorHandle,
  app: HTMLElement,
  type: string,
  files: readonly File[],
): void {
  window.clearTimeout(dropTimer);
  delete app.dataset.drop;
  if (type !== "drop") {
    app.dataset.drop = "";
    dropTimer = window.setTimeout(() => delete app.dataset.drop, 200);
    return;
  }
  const { readOnly } = handle.state();
  const reason = refusal(files, readOnly);
  const [file] = files;
  if (reason !== null) {
    handle.reject({ say: () => refusal(files, readOnly) ?? reason });
  } else if (file) {
    handle.askTables({ kind: "import", file });
  }
}

/**
 * The data tables of a workbook, as far as the editor has them before one is
 * opened: which there are, and the one that was in view.
 */
export function TableDock(props: TableDockProps) {
  const { handle, docId, shown, ask, isCloud, isOnline, isClosed, onTables } = props;
  const [tables, setTables] = useState<readonly TableEntry[]>([]);
  /** Whether the list of them is damaged, or files are left that are not on it. */
  const [isDamaged, setIsDamaged] = useState(false);
  const [views] = useState(() => new TableViews());
  const [remote, setRemote] = useState<RemoteTables | null>(null);
  const [upload, setUpload] = useState<UploadProgress | null>(null);

  // Once more when the workbook is opened again: another tab may have had it meanwhile.
  useEffect(() => {
    let isGone = false;
    if (!isCloud && !isClosed && typeof navigator.storage?.getDirectory === "function") {
      void readCatalog(docId).then(async (found) => {
        const isOdd = found === null || (await hasLeftovers(docId));
        if (!isGone) {
          setTables(found ?? []);
          setIsDamaged(isOdd);
        }
      });
    }
    return () => {
      isGone = true;
    };
  }, [docId, isCloud, isClosed]);

  // Those on the server, and what is on its way there.
  useEffect(() => {
    const target = handle.collab();
    if (isClosed) {
      return;
    }
    if (!isCloud || !target) {
      resumeQueued(loadTables, setUpload).catch(() => {});
      return;
    }
    const { userId, shareToken, role } = target;
    const opened = new RemoteTables(
      { docId, userId, shareToken, canWrite: role !== "viewer" },
      {
        loadUploads: loadTables,
        onTables: setTables,
        onRole: (now) => now && handle.updateCollab({ role: now }),
        onUpload: setUpload,
      },
    );
    setRemote(opened);
    void opened.start();
    return () => {
      opened.stop();
      setRemote(null);
    };
  }, [handle, docId, isCloud, isClosed]);

  useEffect(() => {
    if (isOnline) {
      void remote?.refresh();
    }
  }, [remote, isOnline]);

  useEffect(() => () => handle.showTable(null), [handle]);

  useEffect(() => {
    onTables(isClosed ? [] : tables);
    if (isClosed) {
      handle.showTable(null);
    }
    return () => onTables([]);
  }, [handle, isClosed, tables, onTables]);

  // The table that was in view when the page was left.
  const isRestored = useRef(false);
  useEffect(() => {
    if (!isRestored.current && !isClosed && tables.length > 0) {
      isRestored.current = true;
      const active = views.active(docId);
      if (active !== null && tables.some(({ id }) => id === active)) {
        handle.showTable(active, true);
      }
    }
  }, [handle, docId, isClosed, tables, views]);

  useEffect(() => {
    const rows = askedSample();
    if (rows !== null) {
      handle.askTables({ kind: "sample", docId, rows });
    }
  }, [handle, docId]);

  const onCatalog = useCallback((found: readonly TableEntry[]) => {
    isRestored.current = true;
    setTables(found);
  }, []);

  const isAsked = ask !== null && !isClosed && (ask.kind !== "sample" || ask.docId === docId);
  const isUsed = useRef(false);
  isUsed.current ||= shown !== null || isAsked || isDamaged || upload !== null;
  const [{ Tables, attempt }, setLoad] = useState(() => ({ Tables: lazyTables(), attempt: 0 }));
  const onUpload = () =>
    void (remote ? remote.look(true) : resumeQueued(loadTables, setUpload, true)).catch(() => {});
  const missing = (
    <Missing
      {...props}
      ask={isAsked ? ask : null}
      onRetry={() => setLoad({ Tables: lazyTables(), attempt: attempt + 1 })}
    />
  );
  return isUsed.current && (!isCloud || remote) ? (
    <Boundary key={attempt} fallback={missing}>
      <Suspense fallback={null}>
        <Tables
          {...props}
          ask={isAsked ? ask : null}
          tables={tables}
          isDamaged={isDamaged}
          remote={remote}
          upload={upload}
          views={views}
          onCatalog={onCatalog}
          onUpload={onUpload}
        />
      </Suspense>
    </Boundary>
  ) : null;
}

interface BoundaryProps {
  readonly fallback: ReactNode;
  readonly children: ReactNode;
}

/** Has the rest of the editor go on when what shows tables cannot be had, or has failed. */
class Boundary extends Component<BoundaryProps, { readonly isFailed: boolean }> {
  override state = { isFailed: false };

  static getDerivedStateFromError() {
    return { isFailed: true };
  }

  override render() {
    return this.state.isFailed ? this.props.fallback : this.props.children;
  }
}

/** Takes the place of what could not be loaded. */
function Missing({
  handle,
  shown,
  ask,
  onAsked,
  onRetry,
}: Pick<TableDockProps, "handle" | "shown" | "ask" | "onAsked"> & {
  readonly onRetry: () => void;
}) {
  const buttonRef = useRef<HTMLButtonElement>(null);
  const firstAsk = useRef(ask);
  const latest = useRef({ onAsked, onRetry });
  latest.current = { onAsked, onRetry };
  useEffect(() => {
    if (ask === null) {
      return;
    }
    if (ask === firstAsk.current) {
      // That it could not be loaded has just turned out.
      handle.reject({ say: missingText });
      latest.current.onAsked();
    } else {
      latest.current.onRetry();
    }
  }, [handle, ask]);
  const focus = shown?.focus === true;
  useEffect(() => {
    if (focus) {
      buttonRef.current?.focus();
    }
  }, [focus]);
  return (
    shown && (
      <section className="failure" data-testid="tables-missing">
        <p role="alert">{missingText()}</p>
        <p>
          <button
            ref={buttonRef}
            type="button"
            className="toolbar-btn primary-btn"
            onClick={onRetry}
          >
            {t("Try Again")}
          </button>
        </p>
      </section>
    )
  );
}
