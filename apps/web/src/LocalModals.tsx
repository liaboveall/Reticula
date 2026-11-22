import { useEffect, useRef, useState } from "react";
import type { SafeModeInfo } from "./engine/client.ts";
import { readRawDocFilesFromOpfs, type WorkbookCatalogEntry } from "./engine/storage.ts";

const MAX_CSV_RAW_BYTES = 256 * 1024 * 1024;

export type LocalModalMode =
  | { readonly kind: "workbooks" }
  | { readonly kind: "safeMode"; readonly info: SafeModeInfo };

export interface LocalModalsProps {
  readonly mode: LocalModalMode;
  readonly activeDocId: string;
  readonly docs: readonly WorkbookCatalogEntry[];
  readonly storageUsedMb: string;
  readonly csvProgress: { readonly rowsParsed: number; readonly percent: number } | null;
  readonly onClose: () => void;
  readonly onOpenDoc: (docId: string, title?: string) => void;
  readonly onCreateDoc: (title: string, template: "empty" | "sample") => void;
  readonly onRenameDoc: (docId: string, title: string) => void;
  readonly onDeleteDoc: (docId: string) => void;
  readonly onExportRtcl: () => void;
  readonly onExportCsv: () => void;
  readonly onExportXlsx: () => void;
  readonly onImportRtcl: (title: string, bytes: ArrayBuffer) => void;
  readonly onImportCsv: (title: string, sheetName: string, bytes: ArrayBuffer) => void;
  readonly onImportNotice?: (message: string) => void;
  readonly onImportRejected: (reason: string) => void;
  readonly onSafeModeOpenFrozen: (docId: string) => void;
  readonly onSafeModeSkipTail: (docId: string, skipCount: number, salvageMid: boolean) => void;
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  if (bytes < 1024 * 1024) {
    return `${(bytes / 1024).toFixed(1)} KB`;
  }
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
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
): Uint8Array {
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

export function triggerBrowserDownload(filename: string, bytes: ArrayBuffer | Uint8Array): void {
  const part: BlobPart =
    bytes instanceof Uint8Array
      ? new Uint8Array(
          bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer,
        )
      : bytes;
  const blob = new Blob([part], { type: "application/octet-stream" });
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

export default function LocalModals({
  mode,
  activeDocId,
  docs,
  storageUsedMb,
  csvProgress,
  onClose,
  onOpenDoc,
  onCreateDoc,
  onRenameDoc,
  onDeleteDoc,
  onExportRtcl,
  onExportCsv,
  onExportXlsx,
  onImportRtcl,
  onImportCsv,
  onImportNotice,
  onImportRejected,
  onSafeModeOpenFrozen,
  onSafeModeSkipTail,
}: LocalModalsProps) {
  const fileInputRef = useRef<HTMLInputElement>(null);
  const backdropRef = useRef<HTMLDivElement>(null);
  const [editingDoc, setEditingDoc] = useState<{ id: string; draft: string } | null>(null);
  const [skipCountOverride, setSkipCountOverride] = useState<number | null>(null);
  const [rawExportMessage, setRawExportMessage] = useState<string | null>(null);

  useEffect(() => {
    if (mode.kind !== "workbooks") {
      return;
    }
    const onMouseDown = (event: MouseEvent) => {
      if (event.target === backdropRef.current) {
        onClose();
      }
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !editingDoc) {
        onClose();
      }
    };
    window.addEventListener("mousedown", onMouseDown);
    window.addEventListener("keydown", onKeyDown);
    return () => {
      window.removeEventListener("mousedown", onMouseDown);
      window.removeEventListener("keydown", onKeyDown);
    };
  }, [mode.kind, editingDoc, onClose]);

  if (mode.kind === "safeMode") {
    const { info } = mode;
    const maxSkip = info.tailTxns.length;
    const defaultSkip = info.salvageableMid ? 0 : 1;
    const desiredSkip = skipCountOverride ?? defaultSkip;
    const effectiveSkip =
      maxSkip > 0 ? Math.max(info.salvageableMid ? 0 : 1, Math.min(maxSkip, desiredSkip)) : 0;

    return (
      <div className="modal-backdrop" role="presentation">
        <div
          className="modal-card safe-modal"
          role="dialog"
          aria-modal="true"
          aria-labelledby="safe-mode-heading"
          data-testid="safe-mode-modal"
        >
          <div className="modal-header">
            <h2 id="safe-mode-heading">Safe Mode — {info.title}</h2>
          </div>

          <p className="safe-detail" data-testid="safe-mode-detail">
            {info.detail}
          </p>

          <div className="safe-actions">
            <section className="safe-option">
              <div>
                <strong>1. Open without recalculation</strong>
                <p>
                  Loads the workbook with automatic formula evaluation paused so you can inspect or
                  fix heavy/problematic formulas or export CSV.
                </p>
              </div>
              <button
                type="button"
                className="toolbar-btn primary-btn"
                data-testid="safe-open-frozen"
                onClick={() => onSafeModeOpenFrozen(info.docId)}
              >
                Open Paused
              </button>
            </section>

            <section className="safe-option">
              <div>
                <strong>
                  2.{" "}
                  {info.salvageableMid
                    ? "Recover intact prefix & skip tail transactions"
                    : "Skip recent tail transactions"}
                </strong>
                <p>
                  {maxSkip > 0
                    ? `Discard the last ${effectiveSkip} transaction(s) from the edit log and save a clean snapshot.`
                    : info.salvageableMid
                      ? "Salvage verified transactions before the corrupted log record."
                      : "No tail edit-log transactions are available to skip on this workbook."}
                </p>
                {maxSkip > 0 && (
                  <div className="safe-skip-controls">
                    <label htmlFor="safe-skip-input">Transactions to skip:</label>
                    <input
                      id="safe-skip-input"
                      type="number"
                      min={info.salvageableMid ? 0 : 1}
                      max={maxSkip}
                      value={effectiveSkip}
                      data-testid="safe-skip-count"
                      onChange={(e) => setSkipCountOverride(Number(e.target.value) || 0)}
                    />
                    <span className="safe-muted">of {maxSkip} recent record(s)</span>
                  </div>
                )}
                {info.tailTxns.length > 0 && (
                  <ul className="safe-txn-list" data-testid="safe-txn-list">
                    {info.tailTxns.slice(-6).map((txn) => (
                      <li key={txn.seq}>
                        <code>#{txn.seq}</code> <span>{txn.label}</span>
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
                Recover &amp; Open
              </button>
            </section>

            <section className="safe-option">
              <div>
                <strong>3. Export raw snapshot &amp; edit log</strong>
                <p>
                  Reads the raw OPFS files directly via browser file handles without running the
                  WebAssembly decoder, downloading them to your disk.
                </p>
                {rawExportMessage && (
                  <p className="safe-export-note" data-testid="safe-export-note">
                    {rawExportMessage}
                  </p>
                )}
              </div>
              <button
                type="button"
                className="toolbar-btn"
                data-testid="safe-export-raw"
                onClick={async () => {
                  const files = await readRawDocFilesFromOpfs(info.docId);
                  if (files.length === 0) {
                    setRawExportMessage("No raw OPFS files were found for this workbook.");
                    return;
                  }
                  const zipBytes = packStoreZip(files);
                  triggerBrowserDownload(`${info.docId}-raw-backup.zip`, zipBytes);
                  setRawExportMessage(`Downloaded ${files.length} raw file(s) as .zip.`);
                }}
              >
                Export Raw Files
              </button>
            </section>
          </div>

          <div className="modal-footer">
            <button
              type="button"
              className="toolbar-btn"
              data-testid="safe-new-blank"
              onClick={() => onCreateDoc("Untitled workbook", "empty")}
            >
              Open New Blank Workbook Instead
            </button>
          </div>
        </div>
      </div>
    );
  }

  const handleFilePicked = async (file: File) => {
    const lower = file.name.toLowerCase();
    const baseTitle = file.name.replace(/\.(rtcl|rtclsnap|csv|tsv|txt|xlsx)$/i, "") || "Imported";
    if (lower.endsWith(".rtcl") || lower.endsWith(".rtclsnap")) {
      const buf = await file.arrayBuffer();
      onImportRtcl(baseTitle, buf);
      return;
    }
    if (lower.endsWith(".xlsx")) {
      try {
        const { importXlsxBytes } = await import("./xlsx/xlsxBridge.ts");
        const buf = await file.arrayBuffer();
        const result = await importXlsxBytes(new Uint8Array(buf));
        if (result.report.unsupported.length > 0 || result.report.preservedFormulaCount > 0) {
          const items = result.report.unsupported.map((u) => u.detail);
          if (result.report.preservedFormulaCount > 0) {
            items.push(
              `${result.report.preservedFormulaCount} external/unknown formula(s) preserved with cached values`,
            );
          }
          onImportNotice?.(`Imported with Excel compatibility notes: ${items.join("; ")}`);
        }
        onImportRtcl(baseTitle, result.snapshot);
      } catch (err) {
        onImportRejected(err instanceof Error ? err.message : String(err));
      }
      return;
    }
    if (file.size > MAX_CSV_RAW_BYTES) {
      onImportRejected(
        `CSV file (${formatBytes(file.size)}) exceeds the 256 MiB local workbook limit.`,
      );
      return;
    }
    const buf = await file.arrayBuffer();
    onImportCsv(baseTitle, baseTitle.slice(0, 31) || "Sheet1", buf);
  };

  return (
    <div ref={backdropRef} className="modal-backdrop">
      <div
        className="modal-card drawer-modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="workbooks-heading"
        data-testid="workbooks-drawer"
      >
        <div className="modal-header">
          <h2 id="workbooks-heading">Local Workbooks</h2>
          <button type="button" className="tip-close" aria-label="Close" onClick={onClose}>
            ×
          </button>
        </div>

        <p className="drawer-storage-note">
          Local browser storage only · {storageUsedMb} MB used · Export <code>.rtcl</code> to keep a
          portable backup on disk.
        </p>

        <div className="drawer-toolbar">
          <button
            type="button"
            className="toolbar-btn primary-btn"
            data-testid="new-blank-workbook-btn"
            onClick={() => onCreateDoc(`Workbook ${docs.length + 1}`, "empty")}
          >
            + Blank Workbook
          </button>
          <button
            type="button"
            className="toolbar-btn"
            data-testid="new-sample-workbook-btn"
            onClick={() => onCreateDoc(`Sample 1M (${docs.length + 1})`, "sample")}
          >
            + 1M Sample
          </button>
          <button
            type="button"
            className="toolbar-btn"
            data-testid="import-file-btn"
            onClick={() => fileInputRef.current?.click()}
          >
            Import .rtcl / .csv / .xlsx
          </button>
          <input
            ref={fileInputRef}
            type="file"
            accept=".rtcl,.rtclsnap,.csv,.tsv,.txt,.xlsx"
            data-testid="import-file-input"
            style={{ display: "none" }}
            onChange={(e) => {
              const file = e.target.files?.[0];
              e.target.value = "";
              if (file) {
                void handleFilePicked(file);
              }
            }}
          />
          <span className="titlebar-spacer" />
          <button
            type="button"
            className="toolbar-btn"
            data-testid="drawer-export-rtcl"
            onClick={onExportRtcl}
          >
            Export .rtcl
          </button>
          <button
            type="button"
            className="toolbar-btn"
            data-testid="drawer-export-csv"
            onClick={onExportCsv}
          >
            Export .csv
          </button>
          <button
            type="button"
            className="toolbar-btn"
            data-testid="drawer-export-xlsx"
            onClick={onExportXlsx}
          >
            Export .xlsx
          </button>
        </div>

        {csvProgress && (
          <div className="csv-progress-box" data-testid="csv-import-progress">
            <span>
              Importing CSV: {csvProgress.rowsParsed.toLocaleString("en-US")} rows (
              {csvProgress.percent}%)
            </span>
            <progress max={100} value={csvProgress.percent} />
          </div>
        )}

        <div className="drawer-list" data-testid="workbooks-list">
          {docs.length === 0 ? (
            <p className="drawer-empty">No saved local workbooks yet.</p>
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
                      // biome-ignore lint/a11y/noAutofocus: inline workbook rename requires immediate focus
                      autoFocus
                      onChange={(e) => setEditingDoc({ id: doc.id, draft: e.target.value })}
                      onBlur={() => {
                        const trimmed = editingDoc.draft.trim();
                        setEditingDoc(null);
                        if (trimmed && trimmed !== doc.title) {
                          onRenameDoc(doc.id, trimmed);
                        }
                      }}
                      onKeyDown={(e) => {
                        if (e.key === "Enter") {
                          const trimmed = editingDoc.draft.trim();
                          setEditingDoc(null);
                          if (trimmed && trimmed !== doc.title) {
                            onRenameDoc(doc.id, trimmed);
                          }
                        } else if (e.key === "Escape") {
                          setEditingDoc(null);
                        }
                      }}
                    />
                  ) : (
                    <button
                      type="button"
                      className="drawer-open-btn"
                      data-testid={`open-doc-${doc.id}`}
                      onClick={() => onOpenDoc(doc.id, doc.title)}
                    >
                      <span className="drawer-doc-title">
                        {doc.title}
                        {isCurrent && <span className="drawer-current-pill">Open</span>}
                      </span>
                      <span className="drawer-doc-meta">
                        {formatBytes(doc.sizeBytes)} ·{" "}
                        {new Date(doc.updatedAt).toLocaleTimeString([], {
                          hour: "2-digit",
                          minute: "2-digit",
                        })}
                      </span>
                    </button>
                  )}
                  <button
                    type="button"
                    className="fmt-btn"
                    title="Rename workbook"
                    onClick={() => setEditingDoc({ id: doc.id, draft: doc.title })}
                  >
                    Rename
                  </button>
                  <button
                    type="button"
                    className="fmt-btn"
                    data-testid={`delete-doc-${doc.id}`}
                    title="Delete local workbook"
                    onClick={() => onDeleteDoc(doc.id)}
                  >
                    Delete
                  </button>
                </div>
              );
            })
          )}
        </div>
      </div>
    </div>
  );
}
