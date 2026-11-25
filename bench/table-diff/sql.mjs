// Questions about tables, as SQL that says what they mean.
//
// A table `t` has `rn`, the number of the row in the file, and for every column
// `cN`, the value with its type or NULL. Of texts there is also `lN`, the text
// in lower case, and of all that is stored as an integer, `iN`, that integer.

import { literal } from "./duckdb.mjs";

const isText = (column) => column.kind.type === "text";

/** A whole number that fits into a HUGEINT, and is beyond all others if it did not. */
function clamp(number) {
  const most = 10n ** 37n;
  return number > most ? most : number < -most ? -most : number;
}

/** The whole numbers below and above a decimal number times a power of ten. */
export function around(text, scale) {
  const [, sign, whole, fraction = "", exponent = "0"] =
    /^([+-]?)(\d*)(?:\.(\d*))?(?:[eE]([+-]?\d+))?$/.exec(text);
  const digits = BigInt(`${whole}${fraction}` || "0");
  const value = sign === "-" ? -digits : digits;
  const shift = Number(exponent) + scale - fraction.length;
  if (shift >= 0) {
    const all = clamp(value * 10n ** BigInt(Math.min(shift, 400)));
    return { floor: all, ceiling: all };
  }
  const by = 10n ** BigInt(-shift);
  const floor = value / by - (value % by < 0n ? 1n : 0n);
  const ceiling = floor + (value % by === 0n ? 0n : 1n);
  return { floor: clamp(floor), ceiling: clamp(ceiling) };
}

const double = (text) => {
  const number = Number(text);
  if (Number.isFinite(number)) return `CAST('${number}' AS DOUBLE)`;
  return `CAST('${number > 0 ? "inf" : "-inf"}' AS DOUBLE)`;
};
const huge = (number) => `CAST(${number} AS HUGEINT)`;

function numberCondition(column, at, { op, a, b }) {
  if (column.kind.type === "float") {
    const c = `c${at}`;
    if (op !== "between") return `${c} ${op} ${double(a)}`;
    return `${c} BETWEEN least(${double(a)}, ${double(b)}) AND greatest(${double(a)}, ${double(b)})`;
  }
  // Exactly: a whole number is above another one if it is above its floor.
  const i = `CAST(i${at} AS HUGEINT)`;
  const scale = column.kind.scale ?? 0;
  const one = around(a, scale);
  switch (op) {
    case "=":
      return one.floor === one.ceiling ? `${i} = ${huge(one.floor)}` : "FALSE";
    case "<>":
      return one.floor === one.ceiling ? `${i} <> ${huge(one.floor)}` : `c${at} IS NOT NULL`;
    case ">":
      return `${i} > ${huge(one.floor)}`;
    case ">=":
      return `${i} >= ${huge(one.ceiling)}`;
    case "<":
      return `${i} < ${huge(one.ceiling)}`;
    case "<=":
      return `${i} <= ${huge(one.floor)}`;
    default: {
      const other = around(b, scale);
      const low = one.ceiling < other.ceiling ? one.ceiling : other.ceiling;
      const high = one.floor > other.floor ? one.floor : other.floor;
      return `${i} BETWEEN ${huge(low)} AND ${huge(high)}`;
    }
  }
}

function timeCondition(column, at, { op, a, b }) {
  const c = `c${at}`;
  const date = column.kind.type === "date";
  const moment = (value) =>
    date ? `(DATE '1970-01-01' + CAST(${value} AS INTEGER))` : `make_timestamp(${value})`;
  // A day that a DATE cannot be.
  const inside = (value) => !date || (BigInt(value) > -1_000_000n && BigInt(value) < 4_000_000n);
  if (!inside(a) || (b !== null && !inside(b))) {
    const i = `CAST(i${at} AS HUGEINT)`;
    if (op === "between") {
      return `${i} BETWEEN least(${huge(a)}, ${huge(b)}) AND greatest(${huge(a)}, ${huge(b)})`;
    }
    return `${i} ${op === "on" ? "=" : op} ${huge(a)}`;
  }
  if (op === "between") {
    return `${c} BETWEEN least(${moment(a)}, ${moment(b)}) AND greatest(${moment(a)}, ${moment(b)})`;
  }
  if (op === "on") return `CAST(${c} AS DATE) = CAST(${moment(a)} AS DATE)`;
  return `${c} ${op} ${moment(a)}`;
}

function oneOf(column, at, values) {
  const parts = [];
  // The empty text is a blank.
  if (values.some((value) => value === null || value.text === "")) parts.push(`c${at} IS NULL`);
  const list = values
    .filter((value) => value !== null && value.text !== "")
    .map((value) => {
      if (value.int !== undefined) return huge(value.int);
      if (value.text !== undefined) return literal(value.text.toLowerCase());
      const bits = new DataView(new ArrayBuffer(8));
      bits.setBigUint64(0, BigInt(`0x${value.float}`));
      return double(String(bits.getFloat64(0)));
    });
  const of = isText(column) ? `l${at}` : column.kind.type === "float" ? `c${at}` : `i${at}`;
  if (list.length > 0) parts.push(`coalesce(${of} IN (${list.join(", ")}), FALSE)`);
  return parts.length === 0 ? "FALSE" : `(${parts.join(" OR ")})`;
}

export function condition(columns, [at, of]) {
  const column = columns[at];
  const part = literal((of.text ?? "").toLowerCase());
  switch (of.kind) {
    case "blank":
      return `c${at} IS NULL`;
    case "notBlank":
      return `c${at} IS NOT NULL`;
    case "oneOf":
      return oneOf(column, at, of.values);
    // What nothing is asked of holds for blank cells too.
    case "contains":
      return of.text === "" ? "TRUE" : `contains(l${at}, ${part})`;
    case "starts":
      return of.text === "" ? "TRUE" : `starts_with(l${at}, ${part})`;
    case "ends":
      return of.text === "" ? "TRUE" : `ends_with(l${at}, ${part})`;
    case "number":
      return numberCondition(column, at, of);
    case "time":
      return timeCondition(column, at, of);
    case "top": {
      const order = of.bottom ? "ASC" : "DESC";
      const last = of.bottom ? "max" : "min";
      const sign = of.bottom ? "<=" : ">=";
      return (
        `c${at} ${sign} (SELECT ${last}(x) FROM (SELECT c${at} AS x FROM t ` +
        `WHERE c${at} IS NOT NULL ORDER BY x ${order} LIMIT ${of.n}))`
      );
    }
    default:
      throw new Error(`unknown condition ${of.kind}`);
  }
}

export function where(columns, filters) {
  if (filters.length === 0) return "";
  return `WHERE ${filters.map((filter) => condition(columns, filter)).join(" AND ")}`;
}

export function order(columns, keys) {
  const parts = keys.map(([at, descending]) => {
    const key = isText(columns[at]) ? `l${at}` : `c${at}`;
    return `${key} ${descending ? "DESC" : "ASC"} NULLS LAST`;
  });
  // Rows that are equal stay in the order of the file.
  return `ORDER BY ${[...parts, "rn"].join(", ")}`;
}

/** The value of a column of a file, from its text. */
export function typed(column, text) {
  const { type, scale } = column.kind;
  switch (type) {
    case "integer":
      return `CAST(${text} AS BIGINT)`;
    case "decimal":
      return `CAST(${text} AS DECIMAL(18, ${scale}))`;
    case "float":
      return `CAST(${text} AS DOUBLE)`;
    case "date":
      return `CAST(${text} AS DATE)`;
    case "datetime":
      return `CAST(${text} AS TIMESTAMP)`;
    case "boolean":
      return `CAST(${text} AS BOOLEAN)`;
    default:
      return text;
  }
}

/** The integer that stands for a value. */
export function integer(column, at) {
  const c = `c${at}`;
  switch (column.kind.type) {
    case "integer":
      return c;
    case "decimal":
      return `CAST(replace(CAST(${c} AS VARCHAR), '.', '') AS BIGINT)`;
    case "date":
      return `CAST(datediff('day', DATE '1970-01-01', ${c}) AS BIGINT)`;
    case "datetime":
      return `epoch_us(${c})`;
    case "boolean":
      return `CAST(${c} AS BIGINT)`;
    default:
      return null;
  }
}