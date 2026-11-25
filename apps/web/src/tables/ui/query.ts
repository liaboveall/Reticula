import type {
  TableColumn,
  TableColumnType,
  TableCondition,
  TableQuery,
} from "@reticula/protocol/table";

// What is asked of a table, changed one thing at a time. Nothing here needs a browser.

export type ColumnKind = TableColumnType["kind"];

export const isNumeric = (kind: ColumnKind) =>
  kind === "integer" || kind === "decimal" || kind === "float";
export const isTime = (kind: ColumnKind) => kind === "date" || kind === "dateTime";

/** Whether a condition is for columns of a kind. */
export function applies({ kind }: TableCondition, column: ColumnKind): boolean {
  switch (kind) {
    case "contains":
    case "startsWith":
    case "endsWith":
      return column === "text";
    case "number":
      return isNumeric(column);
    case "time":
      return isTime(column);
    case "top":
      return isNumeric(column) || isTime(column);
    default:
      return true;
  }
}

/** As much of a question as there are columns for. It may be one about another table. */
export function fitting(query: TableQuery, columns: readonly TableColumn[]): TableQuery {
  const seen = new Set<number>();
  return {
    filters: query.filters.filter(({ column, condition }) => {
      const kind = columns[column]?.type.kind;
      return kind !== undefined && applies(condition, kind);
    }),
    order: query.order.filter(
      ({ column }) => column < columns.length && !seen.has(column) && seen.add(column),
    ),
  };
}

export function isPlain({ filters, order }: TableQuery): boolean {
  return filters.length === 0 && order.length === 0;
}

/** Orders by one column alone. */
export function sortedBy(query: TableQuery, column: number, descending: boolean): TableQuery {
  return { ...query, order: [{ column, descending }] };
}

/** Orders by one more column, or by it the other way round if it is ordered by already. */
export function sortedAlsoBy(query: TableQuery, column: number): TableQuery {
  const known = query.order.some((key) => key.column === column);
  const order = known
    ? query.order.map((key) =>
        key.column === column ? { column, descending: !key.descending } : key,
      )
    : [...query.order, { column, descending: false }];
  return { ...query, order };
}

/** Does not order by a column any more, or by any. */
export function unsorted(query: TableQuery, column?: number): TableQuery {
  return {
    ...query,
    order: column === undefined ? [] : query.order.filter((key) => key.column !== column),
  };
}

export function conditionOn(query: TableQuery, column: number): TableCondition | null {
  return query.filters.find((filter) => filter.column === column)?.condition ?? null;
}

/** Sets the condition on a column, or takes it away. */
export function filteredBy(
  query: TableQuery,
  column: number,
  condition: TableCondition | null,
): TableQuery {
  const others = query.filters.filter((filter) => filter.column !== column);
  return { ...query, filters: condition ? [...others, { column, condition }] : others };
}

/** Where something is in a table. Without a row it is a whole column. */
export interface Target {
  readonly row: number | null;
  readonly col: number | null;
}

/** The column that letters stand for, as in a worksheet. */
function columnOfLetters(letters: string): number {
  let col = 0;
  for (const letter of letters.toUpperCase()) {
    col = col * 26 + letter.charCodeAt(0) - 64;
  }
  return col - 1;
}

/** The column that a name or letters stand for. Names come first: they are what is shown. */
function findColumn(text: string, names: readonly string[]): number | null {
  const wanted = text.trim().toLocaleLowerCase();
  const named = names.findIndex((name) => name.trim().toLocaleLowerCase() === wanted);
  if (named >= 0) {
    return named;
  }
  const col = /^[a-z]{1,3}$/.test(wanted) ? columnOfLetters(wanted) : names.length;
  return col < names.length ? col : null;
}

/**
 * What is meant by the number of a row, a column's name or letters, or both, as
 * in `120`, `amount`, `C`, `amount 120` or `C120`. `null` if nothing is.
 */
export function parseTarget(text: string, names: readonly string[], rows: number): Target | null {
  const within = (digits: string) => {
    const row = Number(digits.replace(/[,\s]/g, "")) - 1;
    return row >= 0 && row < rows ? row : null;
  };
  const wanted = text.trim();
  if (/^\d[\d,\s]*$/.test(wanted)) {
    const row = within(wanted);
    return row === null ? null : { row, col: null };
  }
  const col = wanted === "" ? null : findColumn(wanted, names);
  if (col !== null) {
    return { row: null, col };
  }
  const both = /^(.*?\D)\s*(\d+)$/.exec(wanted);
  const column = both && findColumn(both[1] ?? "", names);
  const row = both && within(both[2] ?? "");
  return column === null || row === null ? null : { row, col: column };
}

/** How wide columns are at first, and those that usually have longer texts. */
const WIDTH = 112;
const WIDTHS: Readonly<Partial<Record<ColumnKind, number>>> = {
  float: 150,
  dateTime: 170,
  text: 150,
};

/** What a column's name takes besides its letters, and the most that is made of it. */
const HEADER_ROOM = 48;
const MOST_HEADER = 240;

/** How wide a column is at first: there is room for what is usual, and for its name. */
export function initialWidth({ name, type }: Pick<TableColumn, "name" | "type">): number {
  // Letters of East Asian scripts are as wide as they are high.
  const width = [...name].reduce((sum, letter) => sum + (letter < "⺀" ? 7.5 : 13), HEADER_ROOM);
  return Math.max(WIDTHS[type.kind] ?? WIDTH, Math.min(MOST_HEADER, Math.ceil(width)));
}

/** A cell whose text is not to be taken for a number or a formula, here and elsewhere. */
const TEXT_CELL = `<td data-reticula-text="" style='mso-number-format:"\\@"'>`;

/** Marks the texts of an HTML table, by which of its columns have texts. */
export function markTexts(html: string, isText: readonly boolean[]): string {
  let at = 0;
  return html.replaceAll("<td>", (cell) => (isText[at++ % isText.length] ? TEXT_CELL : cell));
}
