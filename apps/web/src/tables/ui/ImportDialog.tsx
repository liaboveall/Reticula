import type {
  CsvColumnType,
  CsvOptions,
  ParquetInfo,
  TableColumnType,
} from "@reticula/protocol/table";
import { type RefObject, useEffect, useId, useRef, useState, useSyncExternalStore } from "react";
import { flushSync } from "react-dom";
import { DialogHeader } from "../../commands/Dialogs.tsx";
import type { EditorHandle } from "../../commands/handle.ts";
import { fmtBytes } from "../../i18n/formats.ts";
import { fmtInt, t } from "../../i18n/index.ts";
import {
  cloudReason,
  fileKind,
  hasManyRows,
  importWorkbook,
  titleOf,
} from "../../import-workbook.ts";
import { announce } from "../../ui/announcer.ts";
import { Dialog } from "../../ui/Dialog.tsx";
import type { TextEncoding } from "../csv-input.ts";
import type { TableFailure } from "../failure.ts";
import type { ImportedTable, SniffedText } from "../protocol.ts";
import { Progress } from "./Progress.tsx";
import { failureOf, type TableSession } from "./session.ts";
import { failureText, kindText, reportLines } from "./text.ts";

/** What can become a data table. */
const ACCEPTED = ".csv,.tsv,.txt,.parquet,.rtbl";
/** Columns and rows of the preview. Nobody reads more of them in a dialog. */
const MOST_COLUMNS = 100;
const MOST_ROWS = 50;

const ENCODINGS: readonly (readonly [TextEncoding, string])[] = [
  ["utf-8", "UTF-8"],
  ["gb18030", "GB18030"],
  ["utf-16le", "UTF-16 LE"],
  ["utf-16be", "UTF-16 BE"],
  ["windows-1252", "Windows-1252"],
];

/** The types that a column can be told to be of. */
const KINDS: readonly TableColumnType[] = [
  { kind: "text" },
  { kind: "integer" },
  { kind: "decimal", scale: 2, fixed: false },
  { kind: "decimal", scale: 4, fixed: false },
  { kind: "decimal", scale: 6, fixed: false },
  { kind: "float" },
  { kind: "date" },
  { kind: "dateTime" },
  { kind: "boolean" },
];

const keyOf = (type: TableColumnType) =>
  type.kind === "decimal" ? `decimal ${type.scale}` : type.kind;

export interface ImportDialogProps {
  readonly handle: EditorHandle;
  readonly session: TableSession;
  /** A file that was dropped or chosen before, which may become a workbook as well. */
  readonly file: File | null;
  readonly opener: RefObject<Element | null>;
  readonly onFocusGrid: () => void;
  readonly onClose: () => void;
  /** A table was added. */
  readonly onImported: (imported: ImportedTable) => void;
}

/** Makes a data table, or a workbook, out of a file. */
export function ImportDialog({
  handle,
  session,
  file: given,
  opener,
  onFocusGrid,
  onClose,
  onImported,
}: ImportDialogProps) {
  const id = useId();
  const firstRef = useRef<HTMLButtonElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const importing = useSyncExternalStore(session.subscribe, () => session.importing);
  const [file, setFile] = useState(given);
  const [isCloud] = useState(() => handle.collab() !== null);
  const [asTable, setAsTable] = useState(
    given !== null && (isCloud || fileKind(given.name) === "table"),
  );
  const [name, setName] = useState(given ? titleOf(given.name) : "");
  const [options, setOptions] = useState<CsvOptions>({});
  const [encoding, setEncoding] = useState<TextEncoding | undefined>(undefined);
  const [sniffed, setSniffed] = useState<SniffedText | null>(null);
  const [parquet, setParquet] = useState<ParquetInfo | null>(null);
  const [left, setLeft] = useState<ReadonlySet<number>>(new Set());
  const [failure, setFailure] = useState<TableFailure | null>(null);
  const [isRunning, setIsRunning] = useState(false);
  const [done, setDone] = useState<ImportedTable | null>(null);
  const isOpen = useRef(true);

  const kind = file ? fileKind(file.name) : null;
  const isText = kind === "text";
  const isParquet = file !== null && /\.parquet$/i.test(file.name);
  /** Whether there is a choice of what the file becomes. */
  const isChoice = isText && given !== null;

  useEffect(() => {
    isOpen.current = true;
    if (!given) {
      // Where a browser takes this for nobody's wish, the button has the focus.
      inputRef.current?.click();
    }
    return () => {
      isOpen.current = false;
    };
  }, [given]);

  // What the file seems to be better off as.
  useEffect(() => {
    if (isChoice && file && !isCloud) {
      void hasManyRows(file).then(setAsTable, () => {});
    }
  }, [isChoice, file, isCloud]);

  const isPreviewed = file !== null && (asTable || !isChoice);
  useEffect(() => {
    if (!isPreviewed || !file) {
      return;
    }
    let isOver = false;
    const unless =
      <T,>(act: (value: T) => void) =>
      (value: T) =>
        isOver || act(value);
    const fail = unless((error: unknown) => setFailure(failureOf(error)));
    const client = session.started();
    if (isText) {
      client.sniffCsv({ file, encoding, options }).then(
        unless((found: SniffedText) => {
          setSniffed(found);
          setFailure(found.validUtf8 ? null : { code: "notUtf8", row: 1 });
        }),
        fail,
      );
    } else if (isParquet) {
      client.inspectParquet(file).then(unless(setParquet), fail);
    }
    return () => {
      isOver = true;
    };
  }, [session, file, encoding, options, isText, isParquet, isPreviewed]);

  const pick = (picked: File) => {
    setFailure(null);
    setSniffed(null);
    setParquet(null);
    setLeft(new Set());
    setOptions({});
    setEncoding(undefined);
    setName(titleOf(picked.name));
    setFile(picked);
  };

  const start = async () => {
    if (!file) {
      return;
    }
    if (isChoice && !asTable) {
      onClose();
      await importWorkbook(handle, file, { onNotice: handle.notify, onRejected: handle.reject });
      return;
    }
    const title = name.trim().slice(0, 255) || titleOf(file.name);
    setFailure(null);
    setIsRunning(true);
    handle.noteChange();
    try {
      const imported = await session.import(title, (client, task) =>
        isText
          ? client.importCsv({ file, encoding: sniffed?.encoding, options }, title, task)
          : isParquet
            ? client.importParquet(
                file,
                title,
                parquet?.columns.flatMap((_, at) => (left.has(at) ? [] : [at])),
                task,
              )
            : client.importFile(file, title, task),
      );
      const lines = imported.report ? reportLines(imported.report) : [];
      onImported(imported);
      if (isOpen.current && lines.length > 0) {
        setDone(imported);
      } else if (isOpen.current) {
        // It is not said in a dialog that is about to go.
        flushSync(onClose);
        handle.showTable(imported.entry.id);
      }
      announce(
        t("{name} was imported: {rows} rows", {
          name: imported.entry.name,
          rows: fmtInt(imported.entry.rows),
        }),
      );
    } catch (error) {
      const why = failureOf(error);
      if (isOpen.current) {
        setFailure(why);
      } else if (failureText(why)) {
        handle.reject({
          say: () =>
            t("{name} was not imported. {reason}", { name: title, reason: failureText(why) }),
        });
      }
    } finally {
      setIsRunning(false);
    }
  };

  const wrongKind = file !== null && kind === null;
  const isReady =
    file !== null &&
    !wrongKind &&
    !isRunning &&
    ((isChoice && !asTable) ||
      (isText
        ? sniffed !== null
        : !isParquet || (parquet !== null && left.size < parquet.columns.length)));
  const reason = failure && failureText(failure);

  return (
    <Dialog
      labelledBy={id}
      initialFocus={firstRef}
      opener={opener}
      fallbackFocus={onFocusGrid}
      onClose={onClose}
      className="table-dialog"
      testId="table-import"
    >
      <DialogHeader
        id={id}
        text={
          done
            ? t("{name} was imported", { name: done.entry.name })
            : isChoice
              ? t("Import {name}", { name: file?.name ?? "" })
              : t("Import as Data Table")
        }
        onClose={onClose}
      />
      {done ? (
        <>
          <p>
            {t("{rows} rows, {cols} columns", {
              rows: fmtInt(done.entry.rows),
              cols: fmtInt(done.entry.cols),
            })}
          </p>
          <ul className="table-notes" data-testid="table-import-report">
            {(done.report ? reportLines(done.report) : []).map((line) => (
              <li key={line}>{line}</li>
            ))}
          </ul>
          <div className="dialog-footer">
            <button
              type="button"
              className="toolbar-btn primary-btn"
              data-testid="table-import-show"
              onClick={() => {
                onClose();
                handle.showTable(done.entry.id);
              }}
            >
              {t("Show the Table")}
            </button>
          </div>
        </>
      ) : (
        <form
          className="table-form"
          onSubmit={(event) => {
            event.preventDefault();
            if (isReady) {
              void start();
            }
          }}
        >
          {!given && (
            <div className="m3a-row">
              <button
                ref={firstRef}
                type="button"
                className="toolbar-btn"
                data-testid="table-import-choose"
                aria-disabled={isRunning || undefined}
                onClick={() => !isRunning && inputRef.current?.click()}
              >
                {t("Choose a File…")}
              </button>
              <input
                ref={inputRef}
                type="file"
                accept={ACCEPTED}
                hidden
                data-testid="table-import-input"
                onChange={(event) => {
                  const picked = event.target.files?.[0];
                  event.target.value = "";
                  if (picked) {
                    pick(picked);
                  }
                }}
              />
              {file && (
                <span>
                  {file.name} · {fmtBytes(file.size)}
                </span>
              )}
            </div>
          )}
          {wrongKind && (
            <p className="table-failure" role="alert" data-testid="table-import-problem">
              {t("Files of this kind cannot be opened: {name}", { name: file.name })}
            </p>
          )}
          {isChoice && (
            <fieldset className="m3a-choice m3a-row" aria-label={t("Import as")}>
              {([false, true] as const).map((isTable) => (
                <label key={String(isTable)} className="m3a-field">
                  <input
                    type="radio"
                    name={id}
                    checked={asTable === isTable}
                    disabled={isRunning || (isCloud && !isTable)}
                    data-testid={isTable ? "import-as-table" : "import-as-workbook"}
                    onChange={() => setAsTable(isTable)}
                  />
                  <span>
                    {isTable
                      ? t("Data table: read-only, up to 100 million rows, in this workbook")
                      : t("New workbook: cells that can be edited, up to 2 million rows")}
                  </span>
                </label>
              ))}
              {isCloud && (
                <p className="m3a-status" data-testid="table-import-cloud">
                  {cloudReason()}
                </p>
              )}
            </fieldset>
          )}
          {isPreviewed && !wrongKind && (
            <label className="m3a-field">
              <span>{t("Name")}</span>
              <input
                type="text"
                maxLength={255}
                data-testid="table-import-name"
                value={name}
                onChange={(event) => setName(event.target.value)}
              />
            </label>
          )}
          {isPreviewed && isText && sniffed && (
            <CsvPreview
              sniffed={sniffed}
              options={options}
              encoding={encoding ?? sniffed.encoding}
              onOptions={setOptions}
              onEncoding={setEncoding}
            />
          )}
          {isPreviewed && isParquet && parquet && (
            <ParquetPreview info={parquet} left={left} onLeft={setLeft} />
          )}
          {importing && isRunning && (
            <Progress importing={importing} testId="table-import-progress" />
          )}
          {reason && (
            <p className="table-failure" role="alert" data-testid="table-import-failure">
              {reason}
            </p>
          )}
          <div className="dialog-footer">
            {isRunning ? (
              <>
                <button
                  type="button"
                  className="toolbar-btn"
                  data-testid="table-import-cancel"
                  onClick={() => importing?.cancel()}
                >
                  {t("Cancel Import")}
                </button>
                <button type="button" className="toolbar-btn primary-btn" onClick={onClose}>
                  {t("Continue in the Background")}
                </button>
              </>
            ) : (
              <>
                <button
                  ref={given ? firstRef : undefined}
                  type="button"
                  className="toolbar-btn"
                  onClick={onClose}
                >
                  {t("Cancel")}
                </button>
                <button
                  type="submit"
                  className="toolbar-btn primary-btn"
                  data-testid="table-import-start"
                  aria-disabled={!isReady || undefined}
                >
                  {t("Import")}
                </button>
              </>
            )}
          </div>
        </form>
      )}
    </Dialog>
  );
}

const DELIMITERS = [",", "\t", ";", "|"] as const;

function delimiterText(delimiter: string): string {
  switch (delimiter) {
    case ",":
      return t("Comma");
    case "\t":
      return t("Tab|delimiter");
    case ";":
      return t("Semicolon");
    default:
      return t("Vertical bar");
  }
}

/** The first rows of a text file, and how they are read. */
function CsvPreview({
  sniffed,
  options,
  encoding,
  onOptions,
  onEncoding,
}: {
  readonly sniffed: SniffedText;
  readonly options: CsvOptions;
  readonly encoding: TextEncoding;
  readonly onOptions: (options: CsvOptions) => void;
  readonly onEncoding: (encoding: TextEncoding) => void;
}) {
  const shown = sniffed.columns.slice(0, MOST_COLUMNS);
  const typeOf = (column: number) => options.types?.find((type) => type.column === column)?.type;
  const setType = (column: number, type: TableColumnType | undefined) => {
    const others = (options.types ?? []).filter((one) => one.column !== column);
    const own: CsvColumnType[] = type ? [{ column, type }] : [];
    onOptions({ ...options, types: [...others, ...own] });
  };
  return (
    <>
      <div className="m3a-row">
        <label className="m3a-field">
          <span>{t("Encoding")}</span>
          <select
            data-testid="table-import-encoding"
            value={encoding}
            onChange={(event) => onEncoding(event.target.value as TextEncoding)}
          >
            {ENCODINGS.map(([value, text]) => (
              <option key={value} value={value}>
                {text}
              </option>
            ))}
          </select>
        </label>
        <label className="m3a-field">
          <span>{t("Separator")}</span>
          <select
            data-testid="table-import-delimiter"
            value={options.delimiter ?? sniffed.delimiter}
            onChange={(event) => onOptions({ ...options, delimiter: event.target.value })}
          >
            {DELIMITERS.map((delimiter) => (
              <option key={delimiter} value={delimiter}>
                {delimiterText(delimiter)}
              </option>
            ))}
          </select>
        </label>
        <label className="m3a-field">
          <input
            type="checkbox"
            data-testid="table-import-header"
            checked={options.header ?? sniffed.header}
            onChange={(event) => onOptions({ ...options, header: event.target.checked })}
          />
          <span>{t("The first row has the names of the columns")}</span>
        </label>
        <label className="m3a-field">
          <input
            type="checkbox"
            data-testid="table-import-blanks"
            checked={options.blankMarkers === true}
            onChange={(event) => onOptions({ ...options, blankMarkers: event.target.checked })}
          />
          <span>{t("NA, N/A, NULL and NaN are blank")}</span>
        </label>
      </div>
      <section
        className="table-preview"
        aria-label={t("First rows")}
        data-testid="table-import-preview"
        // biome-ignore lint/a11y/noNoninteractiveTabindex: it scrolls
        tabIndex={0}
      >
        <table>
          <thead>
            <tr>
              {shown.map(({ name, type }, column) => {
                const chosen = typeOf(column);
                const kinds =
                  type && !KINDS.some((one) => keyOf(one) === keyOf(type))
                    ? [type, ...KINDS]
                    : KINDS;
                return (
                  // biome-ignore lint/suspicious/noArrayIndexKey: columns have nothing else to tell them apart
                  <th key={column} scope="col">
                    <div>{name}</div>
                    <select
                      aria-label={t("Type of {name}", { name })}
                      data-testid={`table-import-type-${column}`}
                      value={chosen ? keyOf(chosen) : ""}
                      onChange={(event) =>
                        setType(
                          column,
                          kinds.find((one) => keyOf(one) === event.target.value),
                        )
                      }
                    >
                      <option value="">
                        {!chosen && type
                          ? t("Automatic: {type}", { type: kindText(type) })
                          : t("Automatic")}
                      </option>
                      {kinds.map((one) => (
                        <option key={keyOf(one)} value={keyOf(one)}>
                          {kindText(one)}
                        </option>
                      ))}
                    </select>
                  </th>
                );
              })}
            </tr>
          </thead>
          <tbody>
            {sniffed.preview.slice(0, MOST_ROWS).map((row, at) => (
              // biome-ignore lint/suspicious/noArrayIndexKey: rows have nothing else to tell them apart
              <tr key={at}>
                {shown.map((_, column) => (
                  // biome-ignore lint/suspicious/noArrayIndexKey: nor have their cells
                  <td key={column}>{row[column]?.slice(0, 200)}</td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </section>
      {sniffed.columns.length > shown.length && (
        <p className="m3a-status">
          {t("{n} more columns are not shown.", {
            n: fmtInt(sniffed.columns.length - shown.length),
          })}
        </p>
      )}
      <p className="m3a-status">
        {t("A column gets the narrowest type that all of its values fit, unless one is chosen.")}
      </p>
    </>
  );
}

/** The columns of a Parquet file, to choose from. */
function ParquetPreview({
  info,
  left,
  onLeft,
}: {
  readonly info: ParquetInfo;
  /** The columns that are not to be imported. */
  readonly left: ReadonlySet<number>;
  readonly onLeft: (left: ReadonlySet<number>) => void;
}) {
  return (
    <>
      <p className="m3a-status">
        {t("{rows} rows, {cols} columns", {
          rows: fmtInt(info.rows),
          cols: fmtInt(info.columns.length),
        })}
      </p>
      <fieldset
        className="m3a-list table-values"
        aria-label={t("Columns")}
        data-testid="table-import-columns"
      >
        {info.columns.map(({ name, type, bytes }, column) => (
          // biome-ignore lint/suspicious/noArrayIndexKey: columns have nothing else to tell them apart
          <label key={column} className="m3a-field">
            <input
              type="checkbox"
              checked={!left.has(column)}
              onChange={(event) => {
                const next = new Set(left);
                if (event.target.checked) {
                  next.delete(column);
                } else {
                  next.add(column);
                }
                onLeft(next);
              }}
            />
            <span>{name}</span>
            <span className="table-count">
              {kindText(type)} · {fmtBytes(bytes)}
            </span>
          </label>
        ))}
      </fieldset>
      {info.skipped.length > 0 && (
        <ul className="table-notes" data-testid="table-import-skipped">
          {reportLines({
            columns: [],
            skipped: info.skipped,
            extraFields: 0,
            shortRows: 0,
            emptyLines: 0,
          }).map((line) => (
            <li key={line}>{line}</li>
          ))}
        </ul>
      )}
    </>
  );
}
