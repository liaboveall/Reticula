import { columnName, type Selection } from "@reticula/grid";
import type {
  EngineCommand,
  SheetInfo,
  WireColFilterCriterion,
  WireDistinctValue,
  WireFilterCmpOp,
  WireFindOptions,
  WireNamedRangeInfo,
  WireSearchIn,
  WireSearchMatch,
  WireSearchScope,
  WireSortKey,
} from "@reticula/protocol";
import { useEffect, useState } from "react";
import { t } from "./i18n/index.ts";

export type M3aToolMode =
  | { readonly kind: "find"; readonly replace: boolean }
  | { readonly kind: "sort" }
  | { readonly kind: "filter"; readonly col: number }
  | { readonly kind: "names" };

export type M3aModalEvent =
  | {
      readonly kind: "filterValues";
      readonly sheet: number;
      readonly col: number;
      readonly values: readonly WireDistinctValue[];
      readonly blankCount: number;
      readonly truncated: boolean;
    }
  | {
      readonly kind: "searchResults";
      readonly matches: readonly WireSearchMatch[];
      readonly total: number;
      readonly truncated: boolean;
    }
  | { readonly kind: "searchNavigated"; readonly found: WireSearchMatch | undefined }
  | {
      readonly kind: "replaceCompleted";
      readonly replacedCells: number;
      readonly skippedInvalidFormulas: number;
      readonly truncated: boolean;
      readonly sheet: SheetInfo;
    }
  | { readonly kind: "namedRangesListed"; readonly items: readonly WireNamedRangeInfo[] };

/** What came of the latest search or replacement, or `null` for an event about something else. */
function searchOutcome(event: M3aModalEvent): string | null {
  switch (event.kind) {
    case "searchResults":
      return event.truncated
        ? t("Found {n} match(es) (showing first page).", { n: event.total })
        : t("Found {n} match(es).", { n: event.total });
    case "searchNavigated":
      return event.found
        ? t("Jumped to {cell}", {
            cell: `${event.found.sheetName}!${columnName(event.found.col)}${event.found.physicalRow + 1}`,
          })
        : t("No matching cell found.");
    case "replaceCompleted":
      return event.skippedInvalidFormulas > 0
        ? t("Replaced {n} cell(s), skipped {skipped} invalid formula(s).", {
            n: event.replacedCells,
            skipped: event.skippedInvalidFormulas,
          })
        : t("Replaced {n} cell(s).", { n: event.replacedCells });
    default:
      return null;
  }
}

export interface M3aToolsModalProps {
  readonly mode: M3aToolMode;
  readonly sheet: SheetInfo;
  readonly selection: Selection | null;
  readonly readOnly: boolean;
  readonly lastEvent: M3aModalEvent | null;
  readonly onSend: (cmd: EngineCommand) => void;
  readonly onMutate?: (() => void) | undefined;
  readonly onJumpMatch: (match: WireSearchMatch) => void;
  readonly onClose: () => void;
}

export default function M3aToolsModal({
  mode,
  sheet,
  selection,
  readOnly,
  lastEvent,
  onSend,
  onMutate,
  onJumpMatch,
  onClose,
}: M3aToolsModalProps) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        onClose();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div className="modal-backdrop" role="presentation">
      <div className="modal-card" role="dialog" aria-modal="true">
        {mode.kind === "find" && (
          <FindReplacePanel
            initialReplace={mode.replace}
            sheet={sheet}
            selection={selection}
            readOnly={readOnly}
            lastEvent={lastEvent}
            onSend={onSend}
            onMutate={onMutate}
            onJumpMatch={onJumpMatch}
            onClose={onClose}
          />
        )}
        {mode.kind === "sort" && (
          <SortPanel
            sheet={sheet}
            selection={selection}
            readOnly={readOnly}
            onSend={onSend}
            onMutate={onMutate}
            onClose={onClose}
          />
        )}
        {mode.kind === "filter" && (
          <FilterPanel
            initialCol={mode.col}
            sheet={sheet}
            lastEvent={lastEvent}
            onSend={onSend}
            onClose={onClose}
          />
        )}
        {mode.kind === "names" && (
          <NamesPanel
            sheet={sheet}
            selection={selection}
            readOnly={readOnly}
            lastEvent={lastEvent}
            onSend={onSend}
            onMutate={onMutate}
            onClose={onClose}
          />
        )}
      </div>
    </div>
  );
}

function FindReplacePanel({
  initialReplace,
  sheet,
  selection,
  readOnly,
  lastEvent,
  onSend,
  onMutate,
  onJumpMatch,
  onClose,
}: {
  readonly initialReplace: boolean;
  readonly sheet: SheetInfo;
  readonly selection: Selection | null;
  readonly readOnly: boolean;
  readonly lastEvent: M3aModalEvent | null;
  readonly onSend: (cmd: EngineCommand) => void;
  readonly onMutate?: (() => void) | undefined;
  readonly onJumpMatch: (match: WireSearchMatch) => void;
  readonly onClose: () => void;
}) {
  const [query, setQuery] = useState("");
  const [replacement, setReplacement] = useState("");
  const [showReplace, setShowReplace] = useState(initialReplace);
  const [scope, setScope] = useState<WireSearchScope>("activeSheet");
  const [searchIn, setSearchIn] = useState<WireSearchIn>("displayValues");
  const [caseSensitive, setCaseSensitive] = useState(false);
  const [wholeCell, setWholeCell] = useState(false);
  const [useRegex, setUseRegex] = useState(false);
  const [skipHidden, setSkipHidden] = useState(true);
  const [matches, setMatches] = useState<readonly WireSearchMatch[]>([]);
  const [outcome, setOutcome] = useState<M3aModalEvent | null>(null);

  useEffect(() => {
    setShowReplace(initialReplace);
  }, [initialReplace]);

  useEffect(() => {
    if (!lastEvent) {
      return;
    }
    if (lastEvent.kind === "searchResults") {
      setMatches(lastEvent.matches);
    }
    if (searchOutcome(lastEvent) !== null) {
      setOutcome(lastEvent);
    }
  }, [lastEvent]);

  const status = outcome && searchOutcome(outcome);
  const opts: WireFindOptions = {
    query,
    scope,
    searchIn,
    caseSensitive,
    wholeCell,
    useRegex,
    skipHidden,
  };

  const fromRow = selection?.row ?? 0;
  const fromCol = selection?.col ?? 0;

  return (
    <div data-testid="m3a-find-modal" style={{ display: "flex", flexDirection: "column", gap: 10 }}>
      <div className="modal-header">
        <h2>{showReplace ? t("Find & Replace") : t("Find")}</h2>
        <button type="button" className="tip-close" aria-label={t("Close")} onClick={onClose}>
          ×
        </button>
      </div>
      <div className="m3a-row">
        <label className="m3a-field" style={{ flex: 1 }}>
          <span>{t("Find")}</span>
          <input
            type="text"
            data-testid="find-query"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t("Text, pattern, or =formula")}
            style={{ width: "100%" }}
          />
        </label>
        <button type="button" className="toolbar-btn" onClick={() => setShowReplace((v) => !v)}>
          {showReplace ? t("Hide Replace") : t("Replace…")}
        </button>
      </div>
      {showReplace && (
        <label className="m3a-field">
          <span>{t("Replace")}</span>
          <input
            type="text"
            data-testid="replace-input"
            value={replacement}
            onChange={(e) => setReplacement(e.target.value)}
            style={{ flex: 1 }}
          />
        </label>
      )}
      <div className="m3a-row">
        <label className="m3a-field">
          <span>{t("Scope")}</span>
          <select
            data-testid="find-scope"
            value={scope}
            onChange={(e) => setScope(e.target.value as WireSearchScope)}
          >
            <option value="activeSheet">{t("Active sheet")}</option>
            <option value="wholeWorkbook">{t("Whole workbook")}</option>
          </select>
        </label>
        <label className="m3a-field">
          <span>{t("Look in")}</span>
          <select
            data-testid="find-in"
            value={searchIn}
            onChange={(e) => setSearchIn(e.target.value as WireSearchIn)}
          >
            <option value="displayValues">{t("Values")}</option>
            <option value="formulas">{t("Formulas")}</option>
          </select>
        </label>
      </div>
      <div className="m3a-row">
        <label className="m3a-field">
          <input
            type="checkbox"
            checked={caseSensitive}
            onChange={(e) => setCaseSensitive(e.target.checked)}
          />
          <span>{t("Case sensitive")}</span>
        </label>
        <label className="m3a-field">
          <input
            type="checkbox"
            checked={wholeCell}
            onChange={(e) => setWholeCell(e.target.checked)}
          />
          <span>{t("Entire cell")}</span>
        </label>
        <label className="m3a-field">
          <input
            type="checkbox"
            data-testid="find-regex"
            checked={useRegex}
            onChange={(e) => setUseRegex(e.target.checked)}
          />
          <span>{t("Regex")}</span>
        </label>
        <label className="m3a-field">
          <input
            type="checkbox"
            checked={skipHidden}
            onChange={(e) => setSkipHidden(e.target.checked)}
          />
          <span>{t("Skip filtered rows")}</span>
        </label>
      </div>
      <div className="m3a-row">
        <button
          type="button"
          className="toolbar-btn primary-btn"
          data-testid="find-next-btn"
          disabled={!query}
          onClick={() =>
            onSend({
              kind: "findNext",
              sheet: sheet.sheet,
              fromRow,
              fromCol,
              backwards: false,
              options: opts,
            })
          }
        >
          {t("Find Next")}
        </button>
        <button
          type="button"
          className="toolbar-btn"
          data-testid="find-prev-btn"
          disabled={!query}
          onClick={() =>
            onSend({
              kind: "findNext",
              sheet: sheet.sheet,
              fromRow,
              fromCol,
              backwards: true,
              options: opts,
            })
          }
        >
          {t("Previous")}
        </button>
        <button
          type="button"
          className="toolbar-btn"
          data-testid="find-all-btn"
          disabled={!query}
          onClick={() =>
            onSend({ kind: "findAll", sheet: sheet.sheet, options: opts, maxResults: 200 })
          }
        >
          {t("Find All")}
        </button>
        {showReplace && (
          <button
            type="button"
            className="toolbar-btn"
            data-testid="replace-all-btn"
            disabled={readOnly || !query}
            onClick={() => {
              onMutate?.();
              onSend({ kind: "replaceAll", sheet: sheet.sheet, options: opts, replacement });
            }}
          >
            {t("Replace All")}
          </button>
        )}
      </div>
      {status && (
        <p className="m3a-status" data-testid="find-status">
          {status}
        </p>
      )}
      {matches.length > 0 && (
        <div className="m3a-list" data-testid="find-matches-list">
          {matches.map((m) => (
            <button
              key={`${m.sheet}-${m.physicalRow}-${m.col}`}
              type="button"
              className="m3a-match"
              onClick={() => onJumpMatch(m)}
            >
              <code>
                {m.sheetName}!{columnName(m.col)}
                {m.physicalRow + 1}
              </code>
              <span>{m.text}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

interface SortKeyRow {
  readonly id: number;
  readonly colOffset: number;
  readonly ascending: boolean;
  readonly caseSensitive: boolean;
}

function SortPanel({
  sheet,
  selection,
  readOnly,
  onSend,
  onMutate,
  onClose,
}: {
  readonly sheet: SheetInfo;
  readonly selection: Selection | null;
  readonly readOnly: boolean;
  readonly onSend: (cmd: EngineCommand) => void;
  readonly onMutate?: (() => void) | undefined;
  readonly onClose: () => void;
}) {
  const selRange = selection?.range;
  const initFirstRow = selRange ? selRange.firstRow : 0;
  const initLastRow = selRange
    ? Math.max(selRange.firstRow, selRange.endRow - 1)
    : Math.min(sheet.rows - 1, 99);
  const initFirstCol = selRange ? selRange.firstCol : 0;
  const initLastCol = selRange
    ? Math.max(selRange.firstCol, selRange.endCol - 1)
    : Math.min(sheet.cols - 1, 10);

  const [firstRow1, setFirstRow1] = useState(initFirstRow + 1);
  const [lastRow1, setLastRow1] = useState(initLastRow + 1);
  const [firstCol1, setFirstCol1] = useState(initFirstCol + 1);
  const [lastCol1, setLastCol1] = useState(initLastCol + 1);
  const [headerMode, setHeaderMode] = useState<"auto" | "yes" | "no">("auto");
  const [nextKeyId, setNextKeyId] = useState(2);
  const [keys, setKeys] = useState<SortKeyRow[]>([
    { id: 1, colOffset: 0, ascending: true, caseSensitive: false },
  ]);

  const colCount = Math.max(1, lastCol1 - firstCol1 + 1);

  return (
    <div data-testid="m3a-sort-modal" style={{ display: "flex", flexDirection: "column", gap: 10 }}>
      <div className="modal-header">
        <h2>{t("Sort Range")}</h2>
        <button type="button" className="tip-close" aria-label={t("Close")} onClick={onClose}>
          ×
        </button>
      </div>
      <div className="m3a-row">
        <label className="m3a-field">
          <span>{t("Rows (1-based)")}</span>
          <input
            type="number"
            min={1}
            max={sheet.rows}
            data-testid="sort-first-row"
            value={firstRow1}
            onChange={(e) => setFirstRow1(Math.max(1, Number(e.target.value) || 1))}
            style={{ width: 72 }}
          />
          <span>{t("to|range")}</span>
          <input
            type="number"
            min={1}
            max={sheet.rows}
            data-testid="sort-last-row"
            value={lastRow1}
            onChange={(e) => setLastRow1(Math.max(1, Number(e.target.value) || 1))}
            style={{ width: 72 }}
          />
        </label>
        <label className="m3a-field">
          <span>{t("Cols (1-based)")}</span>
          <input
            type="number"
            min={1}
            max={sheet.cols}
            value={firstCol1}
            onChange={(e) => setFirstCol1(Math.max(1, Number(e.target.value) || 1))}
            style={{ width: 58 }}
          />
          <span>{t("to|range")}</span>
          <input
            type="number"
            min={1}
            max={sheet.cols}
            value={lastCol1}
            onChange={(e) => setLastCol1(Math.max(1, Number(e.target.value) || 1))}
            style={{ width: 58 }}
          />
        </label>
      </div>
      <div className="m3a-row">
        <label className="m3a-field">
          <span>{t("Header row")}</span>
          <select
            data-testid="sort-header-mode"
            value={headerMode}
            onChange={(e) => setHeaderMode(e.target.value as "auto" | "yes" | "no")}
          >
            <option value="auto">{t("Auto-detect")}</option>
            <option value="yes">{t("Data has header")}</option>
            <option value="no">{t("No header")}</option>
          </select>
        </label>
        <button
          type="button"
          className="toolbar-btn"
          data-testid="sort-add-key-btn"
          onClick={() => {
            const id = nextKeyId;
            setNextKeyId(id + 1);
            setKeys((prev) => [
              ...prev,
              {
                id,
                colOffset: Math.min(prev.length, colCount - 1),
                ascending: true,
                caseSensitive: false,
              },
            ]);
          }}
        >
          {t("+ Add Level")}
        </button>
      </div>
      <div className="m3a-list">
        {keys.map((k, idx) => (
          <div key={k.id} className="m3a-row">
            <select
              aria-label={t("Sort column {n}", { n: idx + 1 })}
              data-testid={`sort-key-col-${idx}`}
              value={Math.min(k.colOffset, colCount - 1)}
              onChange={(e) => {
                const colOffset = Number(e.target.value);
                setKeys((prev) =>
                  prev.map((item) => (item.id === k.id ? { ...item, colOffset } : item)),
                );
              }}
            >
              {Array.from({ length: colCount }, (_, offset) => {
                const colIdx = firstCol1 - 1 + offset;
                return (
                  <option key={colIdx} value={offset}>
                    {t("Column {name}", { name: columnName(colIdx) })}
                  </option>
                );
              })}
            </select>
            <select
              aria-label={t("Sort order {n}", { n: idx + 1 })}
              data-testid={`sort-key-dir-${idx}`}
              value={k.ascending ? "asc" : "desc"}
              onChange={(e) => {
                const ascending = e.target.value === "asc";
                setKeys((prev) =>
                  prev.map((item) => (item.id === k.id ? { ...item, ascending } : item)),
                );
              }}
            >
              <option value="asc">{t("A → Z / Smallest")}</option>
              <option value="desc">{t("Z → A / Largest")}</option>
            </select>
            {keys.length > 1 && (
              <button
                type="button"
                className="toolbar-btn"
                onClick={() => setKeys((prev) => prev.filter((item) => item.id !== k.id))}
              >
                {t("Remove")}
              </button>
            )}
          </div>
        ))}
      </div>
      <div className="m3a-row" style={{ justifyContent: "flex-end" }}>
        <button type="button" className="toolbar-btn" onClick={onClose}>
          {t("Cancel")}
        </button>
        <button
          type="button"
          className="toolbar-btn primary-btn"
          data-testid="sort-apply-btn"
          disabled={readOnly}
          onClick={() => {
            const r1 = Math.max(0, Math.min(sheet.rows - 1, Math.min(firstRow1, lastRow1) - 1));
            const r2 = Math.max(0, Math.min(sheet.rows - 1, Math.max(firstRow1, lastRow1) - 1));
            const c1 = Math.max(0, Math.min(sheet.cols - 1, Math.min(firstCol1, lastCol1) - 1));
            const c2 = Math.max(0, Math.min(sheet.cols - 1, Math.max(firstCol1, lastCol1) - 1));
            const hasHeader = headerMode === "auto" ? undefined : headerMode === "yes";
            const wireKeys: WireSortKey[] = keys.map(({ colOffset, ascending, caseSensitive }) => ({
              colOffset,
              ascending,
              caseSensitive,
            }));
            onMutate?.();
            onSend({
              kind: "sortRange",
              sheet: sheet.sheet,
              firstRow: r1,
              lastRow: r2,
              firstCol: c1,
              lastCol: c2,
              hasHeader,
              keys: wireKeys,
            });
            onClose();
          }}
        >
          {t("Apply Sort")}
        </button>
      </div>
    </div>
  );
}

function FilterPanel({
  initialCol,
  sheet,
  lastEvent,
  onSend,
  onClose,
}: {
  readonly initialCol: number;
  readonly sheet: SheetInfo;
  readonly lastEvent: M3aModalEvent | null;
  readonly onSend: (cmd: EngineCommand) => void;
  readonly onClose: () => void;
}) {
  const [col, setCol] = useState(initialCol);
  const [modeTab, setModeTab] = useState<"values" | "text" | "number" | "topN">("values");
  const [distinct, setDistinct] = useState<readonly WireDistinctValue[]>([]);
  const [blankCount, setBlankCount] = useState(0);
  const [includeBlanks, setIncludeBlanks] = useState(true);
  const [selected, setSelected] = useState<ReadonlySet<string>>(new Set());
  const [textOp, setTextOp] = useState<"contains" | "startsWith" | "endsWith">("contains");
  const [textNeedle, setTextNeedle] = useState("");
  const [numOp, setNumOp] = useState<WireFilterCmpOp>("gt");
  const [numVal, setNumVal] = useState("0");
  const [topCount, setTopCount] = useState("5");
  const [topDir, setTopDir] = useState(true);

  useEffect(() => {
    setCol(initialCol);
  }, [initialCol]);

  useEffect(() => {
    onSend({ kind: "getFilterValues", sheet: sheet.sheet, col });
  }, [col, onSend, sheet.sheet]);

  useEffect(() => {
    if (
      lastEvent?.kind === "filterValues" &&
      lastEvent.sheet === sheet.sheet &&
      lastEvent.col === col
    ) {
      setDistinct(lastEvent.values);
      setBlankCount(lastEvent.blankCount);
      setSelected(new Set(lastEvent.values.map((v) => v.value)));
      setIncludeBlanks(true);
    }
  }, [col, lastEvent, sheet.sheet]);

  const filterState = sheet.filter;
  const colMin = filterState ? filterState.firstCol : 0;
  const colMax = filterState ? filterState.lastCol : Math.min(sheet.cols - 1, 25);

  const applyCriterion = () => {
    let criterion: WireColFilterCriterion | undefined;
    if (modeTab === "values") {
      criterion = { kind: "values", allowed: [...selected], includeBlanks };
    } else if (modeTab === "text") {
      criterion =
        textOp === "contains"
          ? { kind: "textContains", needle: textNeedle, caseSensitive: false }
          : textOp === "startsWith"
            ? { kind: "textStartsWith", prefix: textNeedle, caseSensitive: false }
            : { kind: "textEndsWith", suffix: textNeedle, caseSensitive: false };
    } else if (modeTab === "number") {
      criterion = { kind: "numberCompare", op: numOp, value: Number(numVal) || 0 };
    } else {
      criterion = { kind: "topN", count: Math.max(1, Number(topCount) || 5), top: topDir };
    }
    onSend({ kind: "setColumnFilter", sheet: sheet.sheet, col, criterion });
    onClose();
  };

  return (
    <div
      data-testid="m3a-filter-modal"
      style={{ display: "flex", flexDirection: "column", gap: 10 }}
    >
      <div className="modal-header">
        <h2>{t("AutoFilter — Column {name}", { name: columnName(col) })}</h2>
        <button type="button" className="tip-close" aria-label={t("Close")} onClick={onClose}>
          ×
        </button>
      </div>
      <div className="m3a-row">
        <label className="m3a-field">
          <span>{t("Column")}</span>
          <select value={col} onChange={(e) => setCol(Number(e.target.value))}>
            {Array.from({ length: Math.max(1, colMax - colMin + 1) }, (_, idx) => {
              const c = colMin + idx;
              return (
                <option key={c} value={c}>
                  {columnName(c)}
                </option>
              );
            })}
          </select>
        </label>
        <div className="m3a-row">
          {(
            [
              ["values", t("Values")],
              ["text", t("Text")],
              ["number", t("Number")],
              ["topN", t("Top N")],
            ] as const
          ).map(([id, label]) => (
            <button
              key={id}
              type="button"
              className={modeTab === id ? "toolbar-btn primary-btn" : "toolbar-btn"}
              onClick={() => setModeTab(id)}
            >
              {label}
            </button>
          ))}
        </div>
      </div>

      {modeTab === "values" && (
        <>
          <div className="m3a-row">
            <button
              type="button"
              className="toolbar-btn"
              onClick={() => {
                setSelected(new Set(distinct.map((d) => d.value)));
                setIncludeBlanks(true);
              }}
            >
              {t("Select All")}
            </button>
            <button
              type="button"
              className="toolbar-btn"
              data-testid="filter-clear-selection-btn"
              onClick={() => {
                setSelected(new Set());
                setIncludeBlanks(false);
              }}
            >
              {t("Unselect All")}
            </button>
          </div>
          <div className="m3a-list" data-testid="filter-values-list">
            {distinct.map((d) => {
              const checked = selected.has(d.value);
              return (
                <label key={d.value} className="m3a-field">
                  <input
                    type="checkbox"
                    data-testid={`filter-val-${d.value}`}
                    checked={checked}
                    onChange={(e) => {
                      const next = new Set(selected);
                      if (e.target.checked) {
                        next.add(d.value);
                      } else {
                        next.delete(d.value);
                      }
                      setSelected(next);
                    }}
                  />
                  <span>
                    {d.value} ({d.count})
                  </span>
                </label>
              );
            })}
            <label className="m3a-field">
              <input
                type="checkbox"
                data-testid="filter-val-blanks"
                checked={includeBlanks}
                onChange={(e) => setIncludeBlanks(e.target.checked)}
              />
              <span>{t("(Blanks) ({n})", { n: blankCount })}</span>
            </label>
          </div>
        </>
      )}

      {modeTab === "text" && (
        <div className="m3a-row">
          <select value={textOp} onChange={(e) => setTextOp(e.target.value as typeof textOp)}>
            <option value="contains">{t("Contains")}</option>
            <option value="startsWith">{t("Begins with")}</option>
            <option value="endsWith">{t("Ends with")}</option>
          </select>
          <input
            type="text"
            data-testid="filter-text-needle"
            value={textNeedle}
            onChange={(e) => setTextNeedle(e.target.value)}
            placeholder={t("Text…")}
          />
        </div>
      )}

      {modeTab === "number" && (
        <div className="m3a-row">
          <select value={numOp} onChange={(e) => setNumOp(e.target.value as WireFilterCmpOp)}>
            <option value="eq">=</option>
            <option value="ne">{"<>"}</option>
            <option value="gt">{">"}</option>
            <option value="ge">{">="}</option>
            <option value="lt">{"<"}</option>
            <option value="le">{"<="}</option>
          </select>
          <input
            type="number"
            data-testid="filter-num-val"
            value={numVal}
            onChange={(e) => setNumVal(e.target.value)}
          />
        </div>
      )}

      {modeTab === "topN" && (
        <div className="m3a-row">
          <select
            value={topDir ? "top" : "bottom"}
            onChange={(e) => setTopDir(e.target.value === "top")}
          >
            <option value="top">{t("Top|filter")}</option>
            <option value="bottom">{t("Bottom|filter")}</option>
          </select>
          <input
            type="number"
            min={1}
            data-testid="filter-topn-count"
            value={topCount}
            onChange={(e) => setTopCount(e.target.value)}
          />
        </div>
      )}

      <div className="m3a-row" style={{ justifyContent: "space-between" }}>
        <div className="m3a-row">
          <button
            type="button"
            className="toolbar-btn"
            data-testid="filter-clear-col-btn"
            onClick={() => {
              onSend({ kind: "setColumnFilter", sheet: sheet.sheet, col, criterion: undefined });
              onClose();
            }}
          >
            {t("Clear Column")}
          </button>
          <button
            type="button"
            className="toolbar-btn"
            data-testid="filter-clear-all-btn"
            onClick={() => {
              onSend({ kind: "clearFilter", sheet: sheet.sheet });
              onClose();
            }}
          >
            {t("Turn Off AutoFilter")}
          </button>
        </div>
        <button
          type="button"
          className="toolbar-btn primary-btn"
          data-testid="filter-apply-btn"
          onClick={applyCriterion}
        >
          {t("Apply Filter")}
        </button>
      </div>
    </div>
  );
}

function NamesPanel({
  sheet,
  selection,
  readOnly,
  lastEvent,
  onSend,
  onMutate,
  onClose,
}: {
  readonly sheet: SheetInfo;
  readonly selection: Selection | null;
  readonly readOnly: boolean;
  readonly lastEvent: M3aModalEvent | null;
  readonly onSend: (cmd: EngineCommand) => void;
  readonly onMutate?: (() => void) | undefined;
  readonly onClose: () => void;
}) {
  const defaultFormula = selection
    ? `=${sheet.sheetName}!$${columnName(selection.range.firstCol)}$${selection.range.firstRow + 1}:$${columnName(Math.max(selection.range.firstCol, selection.range.endCol - 1))}$${Math.max(selection.range.firstRow, selection.range.endRow - 1) + 1}`
    : `=${sheet.sheetName}!$A$1`;
  const [items, setItems] = useState<readonly WireNamedRangeInfo[]>([]);
  const [name, setName] = useState("");
  const [formula, setFormula] = useState(defaultFormula);
  const [useSheetScope, setUseSheetScope] = useState(false);

  useEffect(() => {
    onSend({ kind: "listNamedRanges" });
  }, [onSend]);

  useEffect(() => {
    if (lastEvent?.kind === "namedRangesListed") {
      setItems(lastEvent.items);
    }
  }, [lastEvent]);

  return (
    <div
      data-testid="m3a-names-modal"
      style={{ display: "flex", flexDirection: "column", gap: 10 }}
    >
      <div className="modal-header">
        <h2>{t("Name Manager")}</h2>
        <button type="button" className="tip-close" aria-label={t("Close")} onClick={onClose}>
          ×
        </button>
      </div>
      <div className="m3a-row">
        <label className="m3a-field">
          <span>{t("Name")}</span>
          <input
            type="text"
            data-testid="name-input"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="TAX_RATE"
          />
        </label>
        <label className="m3a-field" style={{ flex: 1 }}>
          <span>{t("Refers to")}</span>
          <input
            type="text"
            data-testid="name-formula-input"
            value={formula}
            onChange={(e) => setFormula(e.target.value)}
            style={{ width: "100%" }}
          />
        </label>
        <label className="m3a-field">
          <input
            type="checkbox"
            checked={useSheetScope}
            onChange={(e) => setUseSheetScope(e.target.checked)}
          />
          <span>{t("{sheet} only", { sheet: sheet.sheetName })}</span>
        </label>
        <button
          type="button"
          className="toolbar-btn primary-btn"
          data-testid="name-save-btn"
          disabled={readOnly || !name.trim() || !formula.trim()}
          onClick={() => {
            onMutate?.();
            onSend({
              kind: "defineNamedRange",
              scope: useSheetScope ? sheet.sheet : undefined,
              name: name.trim(),
              formula: formula.trim(),
              comment: undefined,
              hidden: false,
            });
            setName("");
            onSend({ kind: "listNamedRanges" });
          }}
        >
          {t("Define")}
        </button>
      </div>
      <div className="m3a-list" data-testid="names-list">
        {items.length === 0 && <p className="m3a-status">{t("No defined named ranges yet.")}</p>}
        {items.map((item) => {
          const scopeName =
            item.scope === undefined
              ? t("Workbook|scope")
              : (sheet.sheets.find((s) => s.id === item.scope)?.name ??
                t("Sheet {id}", { id: item.scope }));
          return (
            <div key={`${item.scope ?? "wb"}-${item.name}`} className="m3a-match">
              <span>
                <strong>{item.name}</strong> ({scopeName}) = <code>{item.formula}</code>
              </span>
              <button
                type="button"
                className="toolbar-btn"
                data-testid={`name-delete-${item.name}`}
                disabled={readOnly}
                onClick={() => {
                  onMutate?.();
                  onSend({ kind: "deleteNamedRange", scope: item.scope, name: item.name });
                  onSend({ kind: "listNamedRanges" });
                }}
              >
                {t("Delete")}
              </button>
            </div>
          );
        })}
      </div>
    </div>
  );
}
