import type { ImportReport } from "@reticula/protocol/table";
import { useCallback, useEffect, useId, useRef, useState, useSyncExternalStore } from "react";
import { createPortal } from "react-dom";
import { Confirm, DialogHeader } from "../../commands/Dialogs.tsx";
import type { EditorHandle, TableAsk } from "../../commands/handle.ts";
import { CommandMenu, type Entry } from "../../commands/menus.tsx";
import type { RemoteTables } from "../../editor/remote-tables.ts";
import type { TableDockProps } from "../../editor/TableDock.tsx";
import type { TableViews } from "../../editor/table-views.ts";
import { fmtInt, t } from "../../i18n/index.ts";
import { tx } from "../../i18n/react.ts";
import { fileKind, importWorkbook } from "../../import-workbook.ts";
import { announce } from "../../ui/announcer.ts";
import { Dialog } from "../../ui/Dialog.tsx";
import { useOpener } from "../../ui/opener.ts";
import type { TableEntry } from "../catalog.ts";
import { giveUp } from "../delete.ts";
import type { ImportedTable } from "../protocol.ts";
import type { UploadProgress } from "../uploads.ts";
import { ImportDialog } from "./ImportDialog.tsx";
import { TableModel } from "./model.ts";
import { figureText } from "./Panel.tsx";
import { progressText } from "./Progress.tsx";
import { Screen } from "./Screen.tsx";
import { failureOf, TableSession } from "./session.ts";
import { failureText } from "./text.ts";
import "./tables.css";

const TAB: readonly Entry[] = [
  ["tableRename"],
  ["tableMoveLeft"],
  ["tableMoveRight"],
  ["tableExport"],
  "line",
  ["tableDelete"],
];

/** The rows of the built-in table, and what it is made from. */
const SAMPLE_ROWS = 10_000_000;
const SAMPLE_SEED = 42;
/** Columns whose totals the status bar has room for. */
const MOST_TOTALS = 4;

// One chunk: whatever uses these has a use for the rest.
export { resumeUploads, TableClient } from "../client.ts";

export interface TablesProps extends TableDockProps {
  readonly tables: readonly TableEntry[];
  /** Whether there may be others, or what is left of some, which are looked for. */
  readonly isDamaged: boolean;
  /** Where they are, unless they are in this browser. */
  readonly remote: RemoteTables | null;
  readonly upload: UploadProgress | null;
  readonly views: TableViews;
  readonly onCatalog: (tables: readonly TableEntry[]) => void;
  /** Tries again what could not be uploaded. */
  readonly onUpload: () => void;
}

/** Everything about the data tables of a workbook that is loaded when there is a use for it. */
export default function Tables({
  handle,
  docId,
  tabId,
  shown,
  ask,
  readOnly,
  nameBox,
  zoom,
  tables,
  isDamaged,
  remote,
  upload,
  views,
  onCatalog,
  onUpload,
  onView,
  onAsked,
}: TablesProps) {
  const latest = useRef({ tables, readOnly, shown });
  latest.current = { tables, readOnly, shown };
  const [session] = useState(
    () => new TableSession({ docId, tabId, onCatalog, remote: remote ?? undefined }),
  );
  const [dialog, setDialog] = useState<
    TableAsk | { readonly kind: "upload"; readonly upload: UploadProgress } | null
  >(null);
  /** The table that was deleted last, or whose upload was given up. */
  const [gone, setGone] = useState("");
  const [menu, setMenu] = useState<{ readonly x: number; readonly y: number } | null>(null);
  const [model, setModel] = useState<TableModel | null>(null);
  const [isPanelOpen, setIsPanelOpen] = useState(false);
  const isPanelOpenRef = useRef(false);
  isPanelOpenRef.current = isPanelOpen;
  const dialogOpener = useOpener(dialog !== null);
  const panelOpener = useOpener(isPanelOpen);
  const shownId = shown?.id ?? null;
  const entry = tables.find(({ id }) => id === shownId);

  const refuse = useCallback(
    (error: unknown) => {
      const failure = failureOf(error);
      if (failureText(failure)) {
        handle.reject({ say: () => failureText(failure) });
      }
    },
    [handle],
  );

  useEffect(() => {
    session.reopen();
    return () => session.close();
  }, [session]);

  useEffect(() => {
    if (isDamaged) {
      session.tidy();
    }
  }, [session, isDamaged]);

  const move = useCallback(
    async (id: string, by: -1 | 1) => {
      const { tables: all } = latest.current;
      const at = all.findIndex((table) => table.id === id);
      if (at < 0 || !all[at + by]) {
        return false;
      }
      try {
        await (await session.client()).move(id, at + by);
      } catch (error) {
        refuse(error);
      }
      return true;
    },
    [session, refuse],
  );

  const exportTable = useCallback(
    async (id: string) => {
      try {
        const [{ downloadBlob }, client] = await Promise.all([
          import("../../download.ts"),
          session.client(),
        ]);
        const file = await client.export(id);
        const name = latest.current.tables.find((table) => table.id === id)?.name ?? id;
        downloadBlob(`${name.replace(/[\\/:*?"<>|]+/g, "-")}.rtbl`, file);
        announce(t("{name} was exported.", { name }));
      } catch (error) {
        refuse(error);
      }
    },
    [session, refuse],
  );

  // One for each table that is shown, which goes with it.
  useEffect(() => {
    if (shownId === null) {
      return;
    }
    const own = (command: string) => {
      const { tables: all, readOnly: isFixed } = latest.current;
      const at = all.findIndex(({ id }) => id === shownId);
      switch (command) {
        case "tablePanel":
          return true;
        case "tableExport":
          return at >= 0;
        case "tableRename":
        case "tableDelete":
          return !isFixed;
        case "tableMoveLeft":
          return !isFixed && at > 0;
        case "tableMoveRight":
          return !isFixed && at >= 0 && at < all.length - 1;
        default:
          return /^zoom/.test(command);
      }
    };
    const next = new TableModel({
      id: shownId,
      docId,
      session,
      handle,
      views,
      zoom,
      panel: () => isPanelOpenRef.current,
      canWhole: own,
      runWhole(command) {
        switch (command) {
          case "tablePanel":
            return setIsPanelOpen((isOpen) => !isOpen);
          case "tableRename":
            return setDialog({ kind: "rename", id: shownId });
          case "tableDelete":
            return setDialog({ kind: "delete", id: shownId });
          case "tableExport":
            return exportTable(shownId);
          case "tableMoveLeft":
            return move(shownId, -1);
          case "tableMoveRight":
            return move(shownId, 1);
          default:
            return false;
        }
      },
    });
    setModel(next);
    onView(next);
    return () => {
      setModel(null);
      onView(null);
      // Unless the page, or the workbook, is left with the table in view.
      if (latest.current.shown === null) {
        views.setActive(docId, null);
      }
    };
  }, [shownId, docId, session, handle, views, zoom, onView, move, exportTable]);

  const addSample = useCallback(
    async (rows: number) => {
      const name = t("{n} orders", { n: fmtInt(rows) });
      try {
        // There may be one from before.
        await session.client();
        const known = latest.current.tables.find((table) => table.name === name);
        if (known) {
          handle.showTable(known.id);
          return;
        }
        handle.noteChange();
        const { entry: added } = await session.import(name, (client, task) =>
          client.sample(name, rows, SAMPLE_SEED, task),
        );
        announce(t("{name} was imported: {rows} rows", { name, rows: fmtInt(added.rows) }));
        handle.showTable(added.id);
      } catch (error) {
        refuse(error);
      }
    },
    [session, handle, refuse],
  );

  useEffect(() => {
    if (!ask) {
      return;
    }
    if (ask.kind === "menu" && ask.id !== shownId) {
      // What the menu is about is in view.
      handle.showTable(ask.id, true);
      return;
    }
    switch (ask.kind) {
      case "menu":
        if (!model) {
          return;
        }
        setMenu(ask);
        break;
      case "sample":
        void addSample(ask.rows ?? SAMPLE_ROWS);
        break;
      case "export":
        void exportTable(ask.id);
        break;
      case "move":
        void move(ask.id, ask.by);
        break;
      case "import": {
        const { file } = ask;
        if (file && fileKind(file.name) === "workbook") {
          void importWorkbook(handle, file, {
            onNotice: handle.notify,
            onRejected: handle.reject,
          });
        } else {
          setDialog(ask);
        }
        break;
      }
      default:
        if (readOnly && ask.kind !== "giveUp") {
          announce(t("Nothing can be changed while the workbook is read-only."));
        } else {
          setDialog(ask);
        }
    }
    onAsked();
  }, [ask, shownId, model, readOnly, handle, addSample, exportTable, move, onAsked]);

  const onImported = useCallback(
    ({ entry: { id }, report }: ImportedTable) => {
      if (report) {
        views.write(docId, id, { ...views.read(docId, id), report: withoutPlain(report) });
      }
    },
    [docId, views],
  );

  const common = {
    opener: dialogOpener,
    onFocusGrid: () => focusGrid(handle),
    onClose: () => setDialog(null),
  };
  const asked = dialog && "id" in dialog ? tables.find(({ id }) => id === dialog.id) : undefined;

  return (
    <>
      {model && shown && (
        <Screen
          key={shown.id}
          model={model}
          handle={handle}
          entry={entry}
          shown={shown}
          nameBox={nameBox}
          panel={{
            isOpen: isPanelOpen,
            opener: panelOpener,
            onClose: () => setIsPanelOpen(false),
          }}
        />
      )}
      <Status
        session={session}
        model={model}
        upload={upload?.id === gone ? null : upload}
        onUpload={onUpload}
        onGiveUp={(given) =>
          setDialog(
            given.docId === docId
              ? { kind: "delete", id: given.id }
              : { kind: "upload", upload: given },
          )
        }
      />
      {menu && entry && (
        <CommandMenu
          handle={handle}
          label={entry.name}
          entries={TAB}
          at={{ ...menu, above: true }}
          onClose={() => setMenu(null)}
          testId="table-tab-menu"
        />
      )}
      {dialog?.kind === "import" && (
        <ImportDialog
          {...common}
          handle={handle}
          session={session}
          file={dialog.file ?? null}
          onImported={onImported}
        />
      )}
      {dialog?.kind === "rename" && asked && (
        <RenameDialog {...common} session={session} entry={asked} />
      )}
      {dialog?.kind === "delete" && asked && (
        <Confirm
          {...common}
          heading={t("Delete Data Table")}
          action={t("Delete")}
          testId="table-delete"
          onConfirm={() => {
            setGone(asked.id);
            if (shownId === asked.id) {
              handle.showTable(null);
              handle.cellEditor()?.focus();
            }
            session
              .client()
              .then((client) => client.delete(asked.id))
              .then(() => {
                views.forget(docId, asked.id);
                announce(t('Deleted "{title}".', { title: asked.name }));
              }, refuse);
          }}
        >
          {tx(
            "Delete <0>{name}</0> and its {rows} rows? This cannot be undone.",
            { 0: <strong /> },
            { name: asked.name, rows: fmtInt(asked.rows) },
          )}
        </Confirm>
      )}
      {dialog?.kind === "upload" && (
        <Confirm
          {...common}
          heading={t("Give Up the Upload?")}
          action={t("Give Up")}
          testId="table-upload-give-up"
          onConfirm={() => {
            setGone(dialog.upload.id);
            void giveUp(dialog.upload.docId, dialog.upload.id);
          }}
        >
          {t("{name} will not be on the server, and is lost if it is in no other workbook.", {
            name: dialog.upload.name,
          })}
        </Confirm>
      )}
      {dialog?.kind === "giveUp" && (
        <Confirm
          {...common}
          heading={t("Update Now?")}
          action={t("Update")}
          testId="table-give-up"
          onConfirm={dialog.onAgreed}
        >
          {t("A file is being imported. If the app is updated now, the import is given up.")}
        </Confirm>
      )}
    </>
  );
}

function focusGrid(handle: EditorHandle): void {
  (handle.state().table?.view ?? handle.cellEditor())?.focus();
}

/** A report without the columns there is nothing to say about. */
function withoutPlain(report: ImportReport): ImportReport {
  const { columns } = report;
  return {
    ...report,
    columns: columns.filter(
      ({ misfits, cutTexts, cutNanos, notFinite, outOfRange }) =>
        misfits + cutTexts + cutNanos + notFinite + outOfRange > 0,
    ),
  };
}

/** What the status bar has to say about tables. */
function Status({
  session,
  model,
  upload,
  onUpload,
  onGiveUp,
}: {
  readonly session: TableSession;
  readonly model: TableModel | null;
  readonly upload: UploadProgress | null;
  readonly onUpload: () => void;
  readonly onGiveUp: (upload: UploadProgress) => void;
}) {
  const importing = useSyncExternalStore(session.subscribe, () => session.importing);
  const bar = document.querySelector(".statusbar");
  const { name = "", state, sent = 0, total = 0, failure } = upload ?? {};
  const reason = failure && failureText(failure);
  const percent = Math.floor((sent / Math.max(1, total)) * 100);
  const problem = reason ? t("{name} was not uploaded. {reason}", { name, reason }) : "";
  useEffect(() => {
    announce(problem, { politeness: "assertive" });
  }, [problem]);
  return (
    bar &&
    createPortal(
      <div className="table-status">
        {model && <TableStatus model={model} />}
        {importing && (
          <Job
            testId="table-importing"
            text={progressText(importing)}
            cancel={t("Cancel Import")}
            onCancel={importing.cancel}
          />
        )}
        {upload && (problem || state === "sending" || state === "waiting") && (
          <Job
            testId="table-upload"
            text={
              problem ||
              (state === "waiting"
                ? t("Waiting to upload {name}: {percent}%", { name, percent })
                : t("Uploading {name}: {percent}%", { name, percent }))
            }
            again={problem ? onUpload : undefined}
            cancel={t("Cancel Upload")}
            onCancel={() => onGiveUp(upload)}
          />
        )}
      </div>,
      bar,
    )
  );
}

/** Something that is being done, and how to have it not done. */
function Job({
  testId,
  text,
  again,
  cancel,
  onCancel,
}: {
  readonly testId: string;
  readonly text: string;
  readonly again?: (() => void) | undefined;
  readonly cancel: string;
  readonly onCancel: () => void;
}) {
  return (
    <span className="table-job" data-testid={testId}>
      {text}
      {again && (
        <button
          type="button"
          className="zoom-btn"
          aria-label={t("Try Again")}
          data-testid={`${testId}-again`}
          onClick={again}
        >
          ↻
        </button>
      )}
      <button
        type="button"
        className="zoom-btn"
        aria-label={cancel}
        data-testid={`${testId}-cancel`}
        onClick={onCancel}
      >
        ×
      </button>
    </span>
  );
}

function TableStatus({ model }: { readonly model: TableModel }) {
  const { status, info, rows, job, selection, summaries, here } = useSyncExternalStore(
    model.subscribe,
    model.getState,
  );
  if (!info) {
    return null;
  }
  const selected = selection?.scope === "cols" ? model.selectedColumns() : [];
  const total = fmtInt(info.rows);
  const percent = Math.floor((job?.share ?? 0) * 100);
  const herePercent = here ? Math.floor((here.have / here.total) * 100) : 0;
  return (
    <>
      <span data-testid="table-count">
        {rows === info.rows
          ? t("{n} rows", { n: total })
          : t("{n} of {total} rows", { n: fmtInt(rows), total })}
      </span>
      {selected.slice(0, MOST_TOTALS).map((column) => {
        const summary = summaries.get(column);
        const name = info.columns[column]?.name ?? "";
        return (
          <span key={column} data-testid={`table-total-${column}`}>
            {summary
              ? [
                  name,
                  summary.sum && t("sum {value}", { value: figureText(summary.sum) }),
                  summary.mean && t("mean {value}", { value: figureText(summary.mean) }),
                  summary.min && t("min {value}", { value: figureText(summary.min) }),
                  summary.max && t("max {value}", { value: figureText(summary.max) }),
                  summary.distinct != null && t("{n} distinct", { n: fmtInt(summary.distinct) }),
                  t("{n} blank", { n: fmtInt(summary.blanks) }),
                ]
                  .filter(Boolean)
                  .join(" · ")
              : `${name} · …`}
          </span>
        );
      })}
      {here && here.have < here.total && (
        <span data-testid="table-here">
          {here.isStalled
            ? t("No connection: {percent}% of the table is here", { percent: herePercent })
            : t("{percent}% of the table is here", { percent: herePercent })}
        </span>
      )}
      {job?.kind === "copy" && <span data-testid="table-job">{t("Copying…")}</span>}
      {job && job.kind !== "copy" && (
        <Job
          testId="table-job"
          text={
            job.kind === "fill"
              ? t("Loading all of the table: {percent}%", { percent })
              : t("Sorting and filtering: {percent}%", { percent })
          }
          cancel={t("Cancel What the Table Is Doing")}
          onCancel={() => model.cancel()}
        />
      )}
      {status === "opening" && <span>{t("Opening the data table…")}</span>}
    </>
  );
}

interface CommonProps {
  readonly opener: ReturnType<typeof useOpener>;
  readonly onFocusGrid: () => void;
  readonly onClose: () => void;
}

function RenameDialog({
  opener,
  onFocusGrid,
  onClose,
  session,
  entry,
}: CommonProps & { readonly session: TableSession; readonly entry: TableEntry }) {
  const id = useId();
  const inputRef = useRef<HTMLInputElement>(null);
  const [name, setName] = useState(entry.name);
  const [problem, setProblem] = useState<string | null>(null);
  const rename = async () => {
    try {
      await (await session.client()).rename(entry.id, name);
      onClose();
      announce(t("Renamed to {name}", { name: name.trim() }));
    } catch (error) {
      setProblem(failureText(failureOf(error)));
      inputRef.current?.select();
    }
  };
  return (
    <Dialog
      labelledBy={id}
      initialFocus={inputRef}
      opener={opener}
      fallbackFocus={onFocusGrid}
      onClose={onClose}
      className="small-dialog"
      testId="table-rename"
    >
      <DialogHeader id={id} text={t("Rename Data Table")} onClose={onClose} />
      <form
        className="table-form"
        onSubmit={(event) => {
          event.preventDefault();
          void rename();
        }}
      >
        <input
          ref={inputRef}
          type="text"
          className="table-search"
          aria-label={t("Name")}
          aria-invalid={problem !== null}
          aria-describedby={problem === null ? undefined : `${id}-problem`}
          maxLength={session.isRemote ? 120 : 255}
          data-testid="table-rename-input"
          value={name}
          onFocus={(event) => event.target.select()}
          onChange={(event) => setName(event.target.value)}
        />
        {problem && (
          <p id={`${id}-problem`} className="table-failure" role="alert">
            {problem}
          </p>
        )}
        <div className="dialog-footer">
          <button type="button" className="toolbar-btn" onClick={onClose}>
            {t("Cancel")}
          </button>
          <button
            type="submit"
            className="toolbar-btn primary-btn"
            data-testid="table-rename-confirm"
          >
            {t("Rename")}
          </button>
        </div>
      </form>
    </Dialog>
  );
}
