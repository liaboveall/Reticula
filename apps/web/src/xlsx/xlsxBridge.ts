import {
  exportXlsxSnapshot,
  importXlsx,
  init as initXlsx,
  patchXlsxSnapshot,
  type WireChangedCell,
  type WireXlsxExportWarning,
  type WireXlsxImportReport,
} from "@reticula/protocol/xlsx";
import xlsxWasmUrl from "@reticula/protocol/xlsx-wasm?url";

let ready: Promise<void> | null = null;

/**
 * Loads the module once. A load that failed, as it does without a network, is
 * forgotten, so that the next import or export loads it again.
 */
export async function ensureXlsxReady(): Promise<void> {
  if (!ready) {
    const loading = initXlsx({ module_or_path: xlsxWasmUrl }).then(
      () => undefined,
      (err: unknown) => {
        if (ready === loading) {
          ready = null;
        }
        throw err;
      },
    );
    ready = loading;
  }
  await ready;
}

export interface ImportedXlsxWorkbook {
  readonly snapshot: ArrayBuffer;
  readonly report: WireXlsxImportReport;
}

export interface ExportedXlsxFile {
  readonly bytes: ArrayBuffer;
  readonly warnings: readonly WireXlsxExportWarning[];
}

export async function importXlsxBytes(bytes: Uint8Array): Promise<ImportedXlsxWorkbook> {
  await ensureXlsxReady();
  const bundle = importXlsx(bytes);
  try {
    const snap = bundle.takeSnapshot();
    const copy = new Uint8Array(snap.byteLength);
    copy.set(snap);
    const report = bundle.report();
    return { snapshot: copy.buffer, report };
  } finally {
    bundle.free();
  }
}

export async function exportSnapshotBytesToXlsx(
  snapshotBytes: Uint8Array,
): Promise<ExportedXlsxFile> {
  await ensureXlsxReady();
  const bundle = exportXlsxSnapshot(snapshotBytes);
  try {
    const raw = bundle.takeBytes();
    const copy = new Uint8Array(raw.byteLength);
    copy.set(raw);
    const warnings = bundle.warnings();
    return { bytes: copy.buffer, warnings };
  } finally {
    bundle.free();
  }
}

export async function patchOriginalXlsx(
  origXlsx: Uint8Array,
  snapshotBytes: Uint8Array,
  changedCells: readonly WireChangedCell[],
): Promise<ExportedXlsxFile> {
  await ensureXlsxReady();
  const bundle = patchXlsxSnapshot(origXlsx, snapshotBytes, [...changedCells]);
  try {
    const raw = bundle.takeBytes();
    const copy = new Uint8Array(raw.byteLength);
    copy.set(raw);
    const warnings = bundle.warnings();
    return { bytes: copy.buffer, warnings };
  } finally {
    bundle.free();
  }
}
