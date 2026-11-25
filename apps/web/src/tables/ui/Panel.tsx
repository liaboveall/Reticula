import type { TableColumnSummary, TableFigure } from "@reticula/protocol/table";
import { type ReactNode, type RefObject, useRef, useSyncExternalStore } from "react";
import { fmtBytes } from "../../i18n/formats.ts";
import { fmtInt, t } from "../../i18n/index.ts";
import { useDrawer } from "../../ui/drawer.ts";
import type { TableEntry } from "../catalog.ts";
import type { TableModel } from "./model.ts";
import { filteredBy, unsorted } from "./query.ts";
import { conditionText, kindText, reportLines } from "./text.ts";

export interface PanelProps {
  readonly model: TableModel;
  readonly entry: TableEntry | undefined;
  readonly opener: RefObject<Element | null>;
  readonly onClose: () => void;
}

/** A figure, with a sign before one that is rounded. */
export const figureText = (figure: TableFigure | null | undefined) =>
  figure ? (figure.exact ? "" : "≈ ") + figure.text : "";

const countText = (count: number | null | undefined) => (count == null ? "" : fmtInt(count));

/** Beside the table: what is in its columns, and how it is ordered and filtered. */
export function Panel({ model, entry, opener, onClose }: PanelProps) {
  const { info, rows, query, summaries, report, memory, status } = useSyncExternalStore(
    model.subscribe,
    model.getState,
  );
  const closeRef = useRef<HTMLButtonElement>(null);
  const panelRef = useDrawer({
    open: true,
    initialFocus: closeRef,
    opener,
    onLeave: () => model.focus(),
  });
  const columns = info?.columns ?? [];
  const nameOf = (column: number) => columns[column]?.name ?? "";
  const lines = report ? reportLines(report) : [];

  /** Something that is asked of the table, to be taken away. */
  const asked = (key: string, text: string, label: string, onRemove: () => void): ReactNode => (
    <li key={key}>
      <span>{text}</span>
      <button
        type="button"
        className="tip-close"
        aria-label={label}
        data-testid={`table-remove-${key}`}
        onClick={onRemove}
      >
        ×
      </button>
    </li>
  );

  const figures = (summary: TableColumnSummary | undefined) =>
    [
      ["blanks", countText(summary?.blanks)],
      ["distinct", countText(summary?.distinct)],
      ["sum", figureText(summary?.sum)],
      ["mean", figureText(summary?.mean)],
      ["min", figureText(summary?.min)],
      ["max", figureText(summary?.max)],
    ] as const;

  return (
    <aside
      ref={panelRef}
      className="table-panel"
      aria-label={t("Table panel")}
      data-testid="table-panel"
    >
      <div className="dialog-header">
        <h2>{entry?.name ?? t("Data table")}</h2>
        <button
          ref={closeRef}
          type="button"
          className="tip-close"
          aria-label={t("Close")}
          onClick={onClose}
        >
          ×
        </button>
      </div>
      <p data-testid="table-rows">
        {t("{n} of {total} rows shown", {
          n: fmtInt(rows),
          total: fmtInt(info?.rows ?? entry?.rows ?? 0),
        })}
      </p>

      {query.order.length > 0 && (
        <>
          <h3>{t("Sorted by")}</h3>
          <ol className="table-asked" data-testid="table-order">
            {query.order.map(({ column, descending }) =>
              asked(
                `sort-${column}`,
                descending
                  ? t("{name}, descending", { name: nameOf(column) })
                  : t("{name}, ascending", { name: nameOf(column) }),
                t("Do not sort by {name}", { name: nameOf(column) }),
                () => void model.ask(unsorted(model.asked, column)),
              ),
            )}
          </ol>
        </>
      )}
      {query.filters.length > 0 && (
        <>
          <h3>{t("Filtered by")}</h3>
          <ul className="table-asked" data-testid="table-filters">
            {query.filters.map(({ column, condition }) =>
              asked(
                `filter-${column}`,
                t("{name}: {condition}", {
                  name: nameOf(column),
                  condition: conditionText(condition),
                }),
                t("Do not filter by {name}", { name: nameOf(column) }),
                () => void model.ask(filteredBy(model.asked, column, null)),
              ),
            )}
          </ul>
        </>
      )}

      <h3>{t("Columns")}</h3>
      <section
        className="table-figures"
        aria-label={t("Columns")}
        // biome-ignore lint/a11y/noNoninteractiveTabindex: it scrolls
        tabIndex={0}
      >
        <table aria-busy={status === "open" && summaries.size < columns.length}>
          <thead>
            <tr>
              <th scope="col">{t("Column")}</th>
              <th scope="col">{t("Type")}</th>
              <th scope="col">{t("Blank|count")}</th>
              <th scope="col">{t("Distinct")}</th>
              <th scope="col">{t("Sum")}</th>
              <th scope="col">{t("Mean")}</th>
              <th scope="col">{t("Min")}</th>
              <th scope="col">{t("Max")}</th>
            </tr>
          </thead>
          <tbody>
            {columns.map(({ name, type }, column) => (
              // biome-ignore lint/suspicious/noArrayIndexKey: columns have nothing else to tell them apart
              <tr key={column} data-testid={`table-column-${column}`}>
                <th scope="row">
                  <button type="button" onClick={() => model.goToColumn(column)}>
                    {name}
                  </button>
                </th>
                <td>{kindText(type)}</td>
                {figures(summaries.get(column)).map(([key, text]) => (
                  <td key={key} data-figure={key}>
                    {text}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </section>

      {lines.length > 0 && (
        <>
          <h3>{t("Import report")}</h3>
          <ul data-testid="table-report">
            {lines.map((line) => (
              <li key={line}>{line}</li>
            ))}
          </ul>
        </>
      )}
      <p className="table-size" data-testid="table-size">
        {entry && t("{size} in this browser's storage", { size: fmtBytes(entry.bytes) })}
        {memory !== null && ` · ${t("{size} of memory", { size: fmtBytes(memory) })}`}
      </p>
    </aside>
  );
}
