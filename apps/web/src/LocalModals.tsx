import { type RefObject, useEffect, useRef, useState } from "react";
import { flushSync } from "react-dom";
import type { EditorHandle } from "./commands/handle.ts";
import { runCommand } from "./commands/run.ts";
import { download } from "./download.ts";
import type { SafeModeInfo } from "./engine/client.ts";
import { readRawDocFilesFromOpfs, type WorkbookCatalogEntry } from "./engine/storage.ts";
import { translateEngineText } from "./i18n/engine-text.ts";
import { fmtBytes, fmtDateTime } from "./i18n/formats.ts";
import { type Message, t, tn } from "./i18n/index.ts";
import { tx } from "./i18n/react.ts";
import { fileKind, hasManyRows, importWorkbook } from "./import-workbook.ts";
import { keepsUploads, type TableSize, tableSizes, uploadsReason } from "./local-tables.ts";
import { closeTables } from "./tables/shell.ts";
import { announce } from "./ui/announcer.ts";
import { Dialog } from "./ui/Dialog.tsx";
import { isComposing } from "./ui/focus.ts";

export type LocalModalMode =
  | {
      readonly kind: "workbooks";
      /** Whether it is opened to import a file. */
      readonly importing?: boolean;
    }
  | { readonly kind: "safeMode"; readonly info: SafeModeInfo };

interface WorkbookDialogsProps {
  readonly mode: LocalModalMode;
  /** What had the focus when it was opened. */
  readonly opener: RefObject<Element | null>;
  readonly onFocusGrid: () => void;
  readonly activeDocId: string;
  readonly docs: readonly WorkbookCatalogEntry[];
  readonly storageUsedMb: string;
  readonly onClose: () => void;
  readonly onOpenDoc: (docId: string, title?: string) => void;
  readonly onCreateDoc: (title: string, template: "empty" | "sample") => void;
  readonly onRenameDoc: (docId: string, title: string) => void;
  readonly onDeleteDoc: (docId: string) => void;
  readonly onExportRtcl: () => void;
  readonly onExportCsv: () => void;
  readonly onExportXlsx: () => void;
  readonly onImport: (file: File) => void;
  /** Adds a data table to the workbook that is open, or `null` if it can have none. */
  readonly onImportTable: (() => void) | null;
  readonly onOpenSampleTable: () => void;
  readonly onSafeModeOpenFrozen: (docId: string) => void;
  readonly onSafeModeSkipTail: (docId: string, skipCount: number, salvageMid: boolean) => void;
}

function rawFilesText(count: number): string {
  return count === 0
    ? t("No raw OPFS files were found for this workbook.")
    : t("Downloaded {n} raw file(s) as .zip.", { n: count });
}

function tablesText({ count, bytes }: TableSize = { count: 0, bytes: 0 }): string {
  return tn(count, "{n} data table, {size}", "{n} data tables, {size}", {
    size: fmtBytes(bytes),
  });
}

const CRC32_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let i = 0; i < 256; i++) {
    let c = i;
    for (let k = 0; k < 8; k++) {
      c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    }
    table[i] = c >>> 0;
  }
  return table;
})();

function crc32Bytes(data: Uint8Array): number {
  let crc = 0xffffffff;
  for (let i = 0; i < data.byteLength; i++) {
    const idx = (crc ^ (data[i] ?? 0)) & 0xff;
    crc = (CRC32_TABLE[idx] ?? 0) ^ (crc >>> 8);
  }
  return (crc ^ 0xffffffff) >>> 0;
}

/**
 * Packs raw OPFS rescue files into a standard store-only `.zip` archive so
 * Safe Mode downloads all snapshot & WAL files in a single browser download.
 */
export function packStoreZip(
  files: readonly { readonly filename: string; readonly bytes: Uint8Array }[],
): Uint8Array<ArrayBuffer> {
  const enc = new TextEncoder();
  const entries = files.map((f) => ({
    nameBytes: enc.encode(f.filename),
    bytes: f.bytes,
    crc: crc32Bytes(f.bytes),
    localOffset: 0,
  }));

  let localSize = 0;
  let centralSize = 0;
  for (const e of entries) {
    e.localOffset = localSize;
    localSize += 30 + e.nameBytes.byteLength + e.bytes.byteLength;
    centralSize += 46 + e.nameBytes.byteLength;
  }
  const total = localSize + centralSize + 22;
  const out = new Uint8Array(total);
  const view = new DataView(out.buffer);

  let pos = 0;
  for (const e of entries) {
    view.setUint32(pos, 0x04034b50, true);
    view.setUint16(pos + 4, 20, true);
    view.setUint16(pos + 6, 0x0800, true); // UTF-8 filename flag
    view.setUint16(pos + 8, 0, true); // method 0 = store
    view.setUint16(pos + 10, 0, true);
    view.setUint16(pos + 12, 0x21, true);
    view.setUint32(pos + 14, e.crc, true);
    view.setUint32(pos + 18, e.bytes.byteLength, true);
    view.setUint32(pos + 22, e.bytes.byteLength, true);
    view.setUint16(pos + 26, e.nameBytes.byteLength, true);
    view.setUint16(pos + 28, 0, true);
    out.set(e.nameBytes, pos + 30);
    out.set(e.bytes, pos + 30 + e.nameBytes.byteLength);
    pos += 30 + e.nameBytes.byteLength + e.bytes.byteLength;
  }

  const centralStart = pos;
  for (const e of entries) {
    view.setUint32(pos, 0x02014b50, true);
    view.setUint16(pos + 4, 20, true);
    view.setUint16(pos + 6, 20, true);
    view.setUint16(pos + 8, 0x0800, true);
    view.setUint16(pos + 10, 0, true);
    view.setUint16(pos + 12, 0, true);
    view.setUint16(pos + 14, 0x21, true);
    view.setUint32(pos + 16, e.crc, true);
    view.setUint32(pos + 20, e.bytes.byteLength, true);
    view.setUint32(pos + 24, e.bytes.byteLength, true);
    view.setUint16(pos + 28, e.nameBytes.byteLength, true);
    view.setUint16(pos + 30, 0, true);
    view.setUint16(pos + 32, 0, true);
    view.setUint16(pos + 34, 0, true);
    view.setUint16(pos + 36, 0, true);
    view.setUint32(pos + 38, 0, true);
    view.setUint32(pos + 42, e.localOffset, true);
    out.set(e.nameBytes, pos + 46);
    pos += 46 + e.nameBytes.byteLength;
  }

  view.setUint32(pos, 0x06054b50, true);
  view.setUint16(pos + 4, 0, true);
  view.setUint16(pos + 6, 0, true);
  view.setUint16(pos + 8, entries.length, true);
  view.setUint16(pos + 10, entries.length, true);
  view.setUint32(pos + 12, centralSize, true);
  view.setUint32(pos + 16, centralStart, true);
  view.setUint16(pos + 20, 0, true);
  return out;
}

function WorkbookDialogs({
  mode,
  opener,
  onFocusGrid,
  activeDocId,
  docs,
  storageUsedMb,
  onClose,
  onOpenDoc,
  onCreateDoc,
  onRenameDoc,
  onDeleteDoc,
  onExportRtcl,
  onExportCsv,
  onExportXlsx,
  onImport,
  onImportTable,
  onOpenSampleTable,
  onSafeModeOpenFrozen,
  onSafeModeSkipTail,
}: WorkbookDialogsProps) {
  const fileInputRef = useRef<HTMLInputElement>(null);
  const firstActionRef = useRef<HTMLButtonElement>(null);
  const importRef = useRef<HTMLButtonElement>(null);
  const isImporting = mode.kind === "workbooks" && mode.importing === true;
  const deletingRef = useRef<WorkbookCatalogEntry | null>(null);
  const [editingDoc, setEditingDoc] = useState<{ id: string; draft: string } | null>(null);
  const [skipCountOverride, setSkipCountOverride] = useState<number | null>(null);
  /** How many raw files were downloaded, or `null` while nobody has asked for them. */
  const [rawFileCount, setRawFileCount] = useState<number | null>(null);
  const [tableBytes, setTableBytes] = useState<ReadonlyMap<string, TableSize>>(new Map());

  // Where a browser takes this for nobody's wish, the button has the focus.
  useEffect(() => {
    if (isImporting) {
      fileInputRef.current?.click();
    }
  }, [isImporting]);

  useEffect(() => {
    let isOver = false;
    void tableSizes(docs).then((found) => isOver || setTableBytes(found));
    return () => {
      isOver = true;
    };
  }, [docs]);

  useEffect(() => {
    const deleted = deletingRef.current;
    if (deleted && !docs.some((doc) => doc.id === deleted.id)) {
      deletingRef.current = null;
      announce(t('Deleted "{title}".', { title: deleted.title }));
    }
  }, [docs]);

  if (mode.kind === "safeMode") {
    const { info } = mode;
    const maxSkip = info.tailTxns.length;
    const defaultSkip = info.salvageableMid ? 0 : 1;
    const desiredSkip = skipCountOverride ?? defaultSkip;
    const effectiveSkip =
      maxSkip > 0 ? Math.max(info.salvageableMid ? 0 : 1, Math.min(maxSkip, desiredSkip)) : 0;

    return (
      <Dialog
        key="safeMode"
        labelledBy="safe-mode-heading"
        describedBy="safe-mode-detail"
        closable={false}
        initialFocus={firstActionRef}
        opener={opener}
        fallbackFocus={onFocusGrid}
        onClose={onClose}
        className="safe-modal"
        testId="safe-mode-modal"
      >
        <div className="dialog-header">
          <h2 id="safe-mode-heading">{t("Safe Mode — {title}", { title: info.title })}</h2>
        </div>

        <p id="safe-mode-detail" className="safe-detail" data-testid="safe-mode-detail">
          {translateEngineText(info.detail)}
        </p>

        <div className="safe-actions">
          <section className="safe-option">
            <div>
              <strong>{t("1. Open without recalculation")}</strong>
              <p>
                {t(
                  "Loads the workbook with automatic formula evaluation paused so you can inspect or fix heavy/problematic formulas or export CSV.",
                )}
              </p>
            </div>
            <button
              ref={firstActionRef}
              type="button"
              className="toolbar-btn primary-btn"
              data-testid="safe-open-frozen"
              onClick={() => onSafeModeOpenFrozen(info.docId)}
            >
              {t("Open Paused")}
            </button>
          </section>

          <section className="safe-option">
            <div>
              <strong>
                {info.salvageableMid
                  ? t("2. Recover intact prefix & skip tail transactions")
                  : t("2. Skip recent tail transactions")}
              </strong>
              <p>
                {maxSkip > 0
                  ? t(
                      "Discard the last {n} transaction(s) from the edit log and save a clean snapshot.",
                      { n: effectiveSkip },
                    )
                  : info.salvageableMid
                    ? t("Salvage verified transactions before the corrupted log record.")
                    : t("No tail edit-log transactions are available to skip on this workbook.")}
              </p>
              {maxSkip > 0 && (
                <div className="safe-skip-controls">
                  <label htmlFor="safe-skip-input">{t("Transactions to skip:")}</label>
                  <input
                    id="safe-skip-input"
                    type="number"
                    min={info.salvageableMid ? 0 : 1}
                    max={maxSkip}
                    value={effectiveSkip}
                    data-testid="safe-skip-count"
                    onChange={(e) => setSkipCountOverride(Number(e.target.value) || 0)}
                  />
                  <span className="safe-muted">
                    {t("of {max} recent record(s)", { max: maxSkip })}
                  </span>
                </div>
              )}
              {info.tailTxns.length > 0 && (
                <ul className="safe-txn-list" data-testid="safe-txn-list">
                  {info.tailTxns.slice(-6).map((txn) => (
                    <li key={txn.seq}>
                      <code>#{txn.seq}</code> <span>{translateEngineText(txn.label)}</span>
                    </li>
                  ))}
                </ul>
              )}
            </div>
            <button
              type="button"
              className="toolbar-btn"
              data-testid="safe-skip-tail-btn"
              disabled={maxSkip === 0 && !info.salvageableMid}
              onClick={() => onSafeModeSkipTail(info.docId, effectiveSkip, info.salvageableMid)}
            >
              {t("Recover & Open")}
            </button>
          </section>

          <section className="safe-option">
            <div>
              <strong>{t("3. Export raw snapshot & edit log")}</strong>
              <p>
                {t(
                  "Reads the raw OPFS files directly via browser file handles without running the WebAssembly decoder, downloading them to your disk.",
                )}
              </p>
              {rawFileCount !== null && (
                <p className="safe-export-note" data-testid="safe-export-note">
                  {rawFilesText(rawFileCount)}
                </p>
              )}
            </div>
            <button
              type="button"
              className="toolbar-btn"
              data-testid="safe-export-raw"
              onClick={async () => {
                const files = await readRawDocFilesFromOpfs(info.docId);
                if (files.length > 0) {
                  download(`${info.docId}-raw-backup.zip`, packStoreZip(files));
                }
                setRawFileCount(files.length);
                announce(rawFilesText(files.length));
              }}
            >
              {t("Export Raw Files")}
            </button>
          </section>
        </div>

        <div className="dialog-footer">
          <button
            type="button"
            className="toolbar-btn"
            data-testid="safe-new-blank"
            onClick={() => onCreateDoc("Untitled workbook", "empty")}
          >
            {t("Open New Blank Workbook Instead")}
          </button>
        </div>
      </Dialog>
    );
  }

  const beginRename = (doc: WorkbookCatalogEntry) => {
    setEditingDoc({ id: doc.id, draft: doc.title });
  };

  /** The row's button for renaming has the focus again, unless the user went elsewhere. */
  const endRename = (input: HTMLInputElement, doc: WorkbookCatalogEntry, title: string | null) => {
    const row = input.parentElement;
    const stayed = document.activeElement === input;
    flushSync(() => setEditingDoc(null));
    if (stayed) {
      row?.querySelector<HTMLElement>("[data-rename]")?.focus();
    }
    const trimmed = title?.trim();
    if (trimmed && trimmed !== doc.title) {
      onRenameDoc(doc.id, trimmed);
    }
  };

  return (
    <Dialog
      key="workbooks"
      labelledBy="workbooks-heading"
      closeOnBackdrop
      initialFocus={isImporting ? importRef : firstActionRef}
      opener={opener}
      fallbackFocus={onFocusGrid}
      onClose={onClose}
      testId="workbooks-drawer"
    >
      <div className="dialog-header">
        <h2 id="workbooks-heading">{t("Local Workbooks")}</h2>
        <button type="button" className="tip-close" aria-label={t("Close")} onClick={onClose}>
          ×
        </button>
      </div>

      <p className="drawer-storage-note">
        {tx(
          "Local browser storage only · {size} MB used · Export <0>.rtcl</0> to keep a portable backup on disk.",
          { 0: <code /> },
          { size: storageUsedMb },
        )}
      </p>

      <div className="drawer-toolbar">
        <button
          ref={firstActionRef}
          type="button"
          className="toolbar-btn primary-btn"
          data-testid="new-blank-workbook-btn"
          onClick={() => onCreateDoc(`Workbook ${docs.length + 1}`, "empty")}
        >
          {t("+ Blank Workbook")}
        </button>
        <button
          type="button"
          className="toolbar-btn"
          data-testid="new-sample-workbook-btn"
          onClick={() => onCreateDoc(`Sample 1M (${docs.length + 1})`, "sample")}
        >
          {t("+ 1M Sample")}
        </button>
        <button
          ref={importRef}
          type="button"
          className="toolbar-btn"
          data-testid="import-file-btn"
          onClick={() => fileInputRef.current?.click()}
        >
          {t("Import .rtcl / .csv / .xlsx")}
        </button>
        <input
          ref={fileInputRef}
          type="file"
          accept=".rtcl,.rtclsnap,.csv,.tsv,.txt,.xlsx,.parquet,.rtbl"
          data-testid="import-file-input"
          style={{ display: "none" }}
          onChange={(e) => {
            const file = e.target.files?.[0];
            e.target.value = "";
            if (file) {
              onImport(file);
            }
          }}
        />
        {onImportTable && (
          <button
            type="button"
            className="toolbar-btn"
            data-testid="import-table-btn"
            onClick={onImportTable}
          >
            {t("Import as Data Table…")}
          </button>
        )}
        <button
          type="button"
          className="toolbar-btn"
          data-testid="sample-table-btn"
          onClick={onOpenSampleTable}
        >
          {t("Open 10 Million Rows")}
        </button>
        <span className="titlebar-spacer" />
        <button
          type="button"
          className="toolbar-btn"
          data-testid="drawer-export-rtcl"
          onClick={onExportRtcl}
        >
          {t("Export .rtcl")}
        </button>
        <button
          type="button"
          className="toolbar-btn"
          data-testid="drawer-export-csv"
          onClick={onExportCsv}
        >
          {t("Export .csv")}
        </button>
        <button
          type="button"
          className="toolbar-btn"
          data-testid="drawer-export-xlsx"
          onClick={onExportXlsx}
        >
          {t("Export .xlsx")}
        </button>
      </div>

      {(tableBytes.get(activeDocId)?.count ?? 0) > 0 && (
        <p className="drawer-storage-note" data-testid="tables-not-exported">
          {t(
            "Exported workbooks have no data tables in them. Export each of them as .rtbl, from the menu of its tab.",
          )}
        </p>
      )}

      <div className="drawer-list" data-testid="workbooks-list">
        {docs.length === 0 ? (
          <p className="drawer-empty">{t("No saved local workbooks yet.")}</p>
        ) : (
          docs.map((doc) => {
            const isCurrent = doc.id === activeDocId;
            const isRenaming = editingDoc?.id === doc.id;
            return (
              <div
                key={doc.id}
                className={isCurrent ? "drawer-row current" : "drawer-row"}
                data-testid={`workbook-row-${doc.id}`}
              >
                {isRenaming ? (
                  <input
                    type="text"
                    className="sheet-rename-input"
                    value={editingDoc.draft}
                    autoFocus
                    aria-label={t("New name of {title}", { title: doc.title })}
                    data-testid={`rename-doc-input-${doc.id}`}
                    onChange={(e) => setEditingDoc({ id: doc.id, draft: e.target.value })}
                    onBlur={(e) => endRename(e.currentTarget, doc, editingDoc.draft)}
                    onKeyDown={(e) => {
                      if (isComposing(e.nativeEvent)) {
                        return;
                      }
                      if (e.key === "Enter") {
                        e.preventDefault();
                        endRename(e.currentTarget, doc, editingDoc.draft);
                      } else if (e.key === "Escape") {
                        // It takes the editing back and leaves the dialog open.
                        e.preventDefault();
                        endRename(e.currentTarget, doc, null);
                      }
                    }}
                  />
                ) : (
                  <button
                    type="button"
                    className="drawer-open-btn"
                    aria-current={isCurrent ? "true" : undefined}
                    data-testid={`open-doc-${doc.id}`}
                    onClick={() => onOpenDoc(doc.id, doc.title)}
                  >
                    <span className="drawer-doc-title">
                      {doc.title}
                      {isCurrent && <span className="drawer-current-pill">{t("Open|state")}</span>}
                    </span>
                    <span className="drawer-doc-meta">
                      {fmtBytes(doc.sizeBytes)} ·{" "}
                      {(tableBytes.get(doc.id)?.bytes ?? 0) > 0 &&
                        `${tablesText(tableBytes.get(doc.id))} · `}
                      {fmtDateTime(doc.updatedAt, {
                        hour: "2-digit",
                        minute: "2-digit",
                      })}
                    </span>
                  </button>
                )}
                <button
                  type="button"
                  className="fmt-btn"
                  title={t("Rename workbook")}
                  aria-label={t("Rename {title}", { title: doc.title })}
                  data-rename=""
                  data-testid={`rename-doc-${doc.id}`}
                  onClick={() => beginRename(doc)}
                >
                  {t("Rename")}
                </button>
                <button
                  type="button"
                  className="fmt-btn"
                  data-testid={`delete-doc-${doc.id}`}
                  title={t("Delete local workbook")}
                  aria-label={t("Delete {title}", { title: doc.title })}
                  onClick={() => {
                    deletingRef.current = doc;
                    onDeleteDoc(doc.id);
                  }}
                >
                  {t("Delete")}
                </button>
              </div>
            );
          })
        )}
      </div>
    </Dialog>
  );
}

export type LocalModalsProps = Pick<
  WorkbookDialogsProps,
  "mode" | "opener" | "onFocusGrid" | "activeDocId" | "docs" | "storageUsedMb" | "onClose"
> & {
  readonly handle: EditorHandle;
  readonly onNotice: (message: Message) => void;
  readonly onRejected: (reason: string | Message) => void;
};

/** The same, in the editor. */
export default function LocalModals({ handle, onNotice, onRejected, ...props }: LocalModalsProps) {
  const { onClose } = props;
  return (
    <WorkbookDialogs
      {...props}
      onOpenDoc={(docId, title) => {
        onClose();
        handle.openWith(docId, { kind: "openDoc", docId, title }, handle.collab() !== null);
      }}
      onCreateDoc={(title, template) => handle.createWorkbook(title, template)}
      onRenameDoc={(docId, title) => {
        handle.noteChange();
        handle.sendHost({ kind: "renameDoc", docId, title });
      }}
      onDeleteDoc={async (docId) => {
        if (await keepsUploads(docId)) {
          onClose();
          onRejected({ say: uploadsReason });
          return;
        }
        handle.noteChange();
        // What holds the tables of the workbook would keep it from being deleted.
        if (docId === handle.docId()) {
          handle.showTable(null);
          await closeTables();
        }
        handle.sendHost({ kind: "deleteDoc", docId });
      }}
      onExportRtcl={() => handle.sendHost({ kind: "exportRtcl" })}
      onExportCsv={() => handle.sendHost({ kind: "exportCsv", sheet: handle.sheetId() })}
      onExportXlsx={() => handle.sendHost({ kind: "exportXlsxSnapshot" })}
      onImport={async (file) => {
        const isLarge = fileKind(file.name) === "text" && (await hasManyRows(file));
        if (isLarge || fileKind(file.name) === "table") {
          onClose();
          handle.askTables({ kind: "import", file });
          return;
        }
        await importWorkbook(handle, file, {
          onNotice,
          onRejected: (reason) => {
            onClose();
            onRejected(reason);
          },
          onOpening: onClose,
        });
      }}
      onImportTable={
        handle.state().readOnly
          ? null
          : () => {
              onClose();
              handle.askTables({ kind: "import" });
            }
      }
      onOpenSampleTable={() => {
        onClose();
        runCommand(handle, "sampleTable");
      }}
      onSafeModeOpenFrozen={(docId) => {
        onClose();
        handle.sendHost({ kind: "safeModeRecover", docId, mode: { type: "openFrozen" } });
      }}
      onSafeModeSkipTail={(docId, skipCount, salvageMid) => {
        onClose();
        handle.sendHost({
          kind: "safeModeRecover",
          docId,
          mode: { type: "skipTail", skipCount, salvageMid },
        });
      }}
    />
  );
}
