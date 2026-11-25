import type { EditorHandle } from "./commands/handle.ts";
import type { HostCommand } from "./engine/client.ts";
import { translateEngineText } from "./i18n/engine-text.ts";
import { type Message, t } from "./i18n/index.ts";

/** Most bytes of a text file that can become a workbook. */
export const MAX_CSV_RAW_BYTES = 256 * 1024 * 1024;
/** Rows that make a data table the better place for a text file. */
const MANY_ROWS = 1_000_000;
/** How much of a text file is looked at to estimate its rows. */
const HEAD_BYTES = 64 * 1024;

const WORKBOOK = /\.(?:rtcl|rtclsnap|xlsx)$/i;
const TEXT = /\.(?:csv|tsv|txt)$/i;
const TABLE = /\.(?:parquet|rtbl)$/i;

/** What a file can become, by its name. */
export function fileKind(name: string): "workbook" | "text" | "table" | null {
  return WORKBOOK.test(name)
    ? "workbook"
    : TEXT.test(name)
      ? "text"
      : TABLE.test(name)
        ? "table"
        : null;
}

/** A file's name without what says its kind. */
export function titleOf(name: string): string {
  return name.replace(/\.(?:rtcl|rtclsnap|csv|tsv|txt|xlsx|parquet|rtbl)$/i, "") || "Imported";
}

/** Whether a text file has so many rows that it should be a data table. */
export async function hasManyRows(file: Blob): Promise<boolean> {
  if (file.size > MAX_CSV_RAW_BYTES) {
    return true;
  }
  if (file.size <= HEAD_BYTES) {
    return false;
  }
  const head = new Uint8Array(await file.slice(0, HEAD_BYTES).arrayBuffer());
  const lines = head.reduce((count, byte) => (byte === 10 ? count + 1 : count), 0);
  return (lines * file.size) / HEAD_BYTES > MANY_ROWS;
}

/** Why no file becomes a workbook while one on the server is open. */
export const cloudReason = () =>
  t("Open a local workbook first: a file becomes a workbook in this browser.");

/** What somebody else has to do while a workbook is imported. */
export interface WorkbookImport {
  readonly onNotice: (message: Message) => void;
  readonly onRejected: (reason: string | Message) => void;
  /** It is being opened, and how far it has got is shown. */
  readonly onOpening?: () => void;
}

/** Opens a file as a new local workbook. */
export async function importWorkbook(
  handle: EditorHandle,
  file: File,
  { onNotice, onRejected, onOpening }: WorkbookImport,
): Promise<void> {
  if (handle.collab()) {
    onRejected({ say: cloudReason });
    return;
  }
  const title = titleOf(file.name);
  const open = (command: HostCommand & { readonly bytes: ArrayBuffer }) => {
    // What is being typed belongs to the workbook that is open.
    handle.commitPending();
    handle.noteChange();
    onOpening?.();
    handle.sendHost(command, [command.bytes]);
  };
  const openRtcl = (bytes: ArrayBuffer) =>
    open({ kind: "importRtcl", docId: handle.freshDocId(), title, bytes });
  if (/\.xlsx$/i.test(file.name)) {
    try {
      const { importXlsxBytes } = await import("./xlsx/xlsxBridge.ts");
      const buf = await file.arrayBuffer();
      const result = await importXlsxBytes(new Uint8Array(buf));
      const { unsupported, preservedFormulaCount: n } = result.report;
      if (unsupported.length > 0 || n > 0) {
        const items = () => [
          ...unsupported.map((u) => translateEngineText(u.detail)),
          ...(n > 0
            ? [t("{n} external/unknown formula(s) preserved with cached values", { n })]
            : []),
        ];
        onNotice({
          say: () =>
            t("Imported with Excel compatibility notes: {notes}", {
              notes: items().join(t("; |separator")),
            }),
        });
      }
      openRtcl(result.snapshot);
    } catch (err) {
      onRejected(err instanceof Error ? err.message : String(err));
    }
    return;
  }
  if (WORKBOOK.test(file.name)) {
    openRtcl(await file.arrayBuffer());
    return;
  }
  if (file.size > MAX_CSV_RAW_BYTES) {
    const size = `${(file.size / 1024 ** 2).toFixed(1)} MB`;
    onRejected({
      say: () => t("CSV file ({size}) exceeds the 256 MiB local workbook limit.", { size }),
    });
    return;
  }
  const bytes = await file.arrayBuffer();
  const sheetName = title.slice(0, 31) || "Sheet1";
  open({ kind: "importCsv", docId: handle.freshDocId(), title, sheetName, bytes });
}
