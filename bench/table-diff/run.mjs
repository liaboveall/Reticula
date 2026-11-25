// Asks DuckDB what the engine was asked, and compares the answers.
//
//   node bench/table-diff/run.mjs parquet <dir>
//   node bench/table-diff/run.mjs compare <dir>
//
// `cargo xtask diff --tables` runs this after the native half, in a process that
// may read the repository, write to <dir>, and do nothing else: the home
// directory is one of its own, and there is no network.

import { readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { FLOAT_SUM, FOLDING, LISTED, REFUSED, TYPE_DIFFERENCES } from "./allowed.mjs";
import { DUCKDB_VERSION, literal, name, open } from "./duckdb.mjs";
import { comesBack, compareBytes, exactSum, isMean, isShown, isSum, shown } from "./format.mjs";
import { integer, order, typed, where } from "./sql.mjs";

const [stage, directory] = process.argv.slice(2);
const dir = resolve(directory);
const listPath = join(dir, "tables.json");
const list = JSON.parse(readFileSync(listPath, "utf8"));
// Where the files are that DuckDB reads.
process.chdir(dir);

const stem = (table) => table.name.replaceAll(" ", "-");

/** What the native half wrote about a table. */
function answers(table, from) {
  const text = readFileSync(join(dir, `${stem(table)}.${from}.jsonl`), "utf8");
  const [meta, ...questions] = text
    .trimEnd()
    .split("\n")
    .map((line) => JSON.parse(line));
  const bytes = readFileSync(join(dir, `${stem(table)}.${from}.bin`));
  return { meta, questions, bytes };
}

/** Texts that are blank cells where that was asked for. */
const MARKERS = ["", "NA", "na", "N/A", "n/a", "NULL", "null", "NaN", "nan", "NAN"];

function csvOptions({ delimiter, markers }) {
  // The empty text is a blank, quoted or not.
  const blanks = markers ? `[${MARKERS.map(literal).join(", ")}]` : "''";
  return (
    `header = true, delim = ${literal(delimiter)}, quote = '"', escape = '"', nullstr = ${blanks}, ` +
    "null_padding = true, parallel = false, max_line_size = 4000000"
  );
}

/** The fields of a delimited file as texts. */
function createRaw(db, table) {
  const columns = table.columns.map((_, at) => `'f${at}': 'VARCHAR'`).join(", ");
  db.run(
    `CREATE TABLE raw AS SELECT row_number() OVER () - 1 AS rn, * FROM read_csv(${literal(table.csv)}, ` +
      `${csvOptions(table)}, auto_detect = false, all_varchar = true, columns = {${columns}})`,
  );
}

/** Makes `t` of the values, see sql.mjs. Gives how many texts are folded in another way. */
function createTable(db, columns, values, source) {
  const selected = columns.map((column, at) => `${values(column, at)} AS c${at}`);
  db.run(`CREATE TABLE v AS SELECT rn, ${selected.join(", ")} FROM ${source}`);
  const more = [];
  const joins = [];
  let folded = 0;
  for (const [at, column] of columns.entries()) {
    const stored = integer(column, at);
    if (stored !== null) more.push(`${stored} AS i${at}`);
    if (column.kind.type !== "text") continue;
    const texts = db.values(
      `SELECT DISTINCT c${at} AS c, lower(c${at}) FROM v WHERE length(c${at}) <> strlen(c${at})`,
    );
    const others = [];
    for (const [index, text] of texts[0].entries()) {
      const lower = text.toLowerCase();
      if (lower === texts[1][index]) continue;
      if (!FOLDING.pattern.test(text)) {
        throw new Error(
          `${JSON.stringify(text)} is ${JSON.stringify(texts[1][index])} in lower case`,
        );
      }
      others.push(`(${literal(text)}, ${literal(lower)})`);
    }
    if (others.length === 0) {
      more.push(`lower(c${at}) AS l${at}`);
      continue;
    }
    folded += others.length;
    db.run(`CREATE TABLE fold${at} (k VARCHAR, f VARCHAR)`);
    db.run(`INSERT INTO fold${at} VALUES ${others.join(", ")}`);
    joins.push(`LEFT JOIN fold${at} ON fold${at}.k = v.c${at}`);
    more.push(`coalesce(fold${at}.f, lower(c${at})) AS l${at}`);
  }
  const all = ["v.*", ...more].join(", ");
  db.run(`CREATE TABLE t AS SELECT ${all} FROM v ${joins.join(" ")} ORDER BY rn`);
  return folded;
}

function dropAll(db) {
  for (const { table_name } of db.rows("SELECT table_name FROM duckdb_tables()")) {
    db.run(`DROP TABLE ${name(table_name)}`);
  }
}

// Parquet files.

/** What only Parquet files have. The columns are numbers of rows made into values. */
const PARQUET_ONLY = {
  "parquet types": `SELECT
      CAST(r % 200 - 100 AS TINYINT) AS tiny,
      CAST(r % 60000 - 30000 AS SMALLINT) AS small,
      CASE WHEN r % 7 > 0 THEN CAST(r * 1000 - 5 AS INTEGER) END AS "int",
      CAST(r % 256 AS UTINYINT) AS utiny,
      CAST(r % 65536 AS USMALLINT) AS usmall,
      CAST(4294967295 - r AS UINTEGER) AS uint,
      CAST(r * 1000000007 AS UBIGINT) AS ubig,
      CASE WHEN r % 3 = 0 THEN 18446744073709551615 - CAST(r AS UBIGINT) ELSE CAST(r AS UBIGINT) END AS ubig_over,
      CAST(r % 1000 / 10.0 AS DECIMAL(4, 1)) AS dec4,
      CAST(-r / 100.0 AS DECIMAL(9, 2)) AS dec9,
      CASE WHEN r % 5 > 0 THEN CAST(r * 12345.678901 AS DECIMAL(18, 6)) END AS dec18,
      CAST(r AS DECIMAL(38, 10)) / 7 AS dec_div,
      CAST(CAST(r AS VARCHAR) || '123456789012345678.0123456789' AS DECIMAL(38, 10)) AS dec38,
      CASE r % 11 WHEN 0 THEN 'NaN' WHEN 1 THEN 'inf' WHEN 2 THEN '-inf' ELSE CAST(r / 8 AS VARCHAR) END::FLOAT AS f32,
      CASE r % 13 WHEN 0 THEN 'NaN' WHEN 1 THEN 'inf' WHEN 2 THEN '-0.0' ELSE CAST(r / 3 AS VARCHAR) END::DOUBLE AS f64,
      DATE '0001-01-01' + CAST(r * 41 AS INTEGER) AS "date",
      make_timestamp(r * 123456789 - 5000000000000) AS ts,
      CAST(make_timestamp(r * 1000000) AS TIMESTAMP_S) AS ts_s,
      CAST(make_timestamp(r * 1001000) AS TIMESTAMP_MS) AS ts_ms,
      make_timestamp_ns(r * 1000000007 - 70000000000) AS ts_ns,
      CAST('2024-01-01 00:00:00+00' AS TIMESTAMPTZ) AS ts_tz,
      CASE WHEN r % 9 > 0 THEN TIME '00:00:00' + to_microseconds(r * 1000007 % 86400000000) END AS "time",
      CAST(printf('%08x-0000-4000-8000-%012x', CAST(r AS BIGINT), CAST(r * 7 AS BIGINT)) AS UUID) AS "uuid",
      CAST('blob ' || r % 50 AS BLOB) AS "blob",
      CAST(['red', 'Green', 'BLUE'][r % 3 + 1] AS ENUM ('red', 'Green', 'BLUE')) AS "enum",
      CASE WHEN r % 4 > 0 THEN r % 2 = 0 END AS truth,
      CASE r % 6 WHEN 0 THEN '' WHEN 1 THEN NULL WHEN 2 THEN 'Müller ' || r % 40 ELSE 'x' || r % 9000 END AS text,
      NULL::INTEGER AS nothing,
      [r, r + 1] AS "list",
      {'a': r, 'b': 'x'} AS "struct"
    FROM (SELECT range AS r FROM range(ROWS))`,
};

function copyOptions({ compression, rowGroup, version }) {
  return `FORMAT parquet, COMPRESSION ${compression}, ROW_GROUP_SIZE ${rowGroup}, PARQUET_VERSION ${version}`;
}

function writeParquet(db) {
  for (const table of list.tables) {
    if (table.parquet === null) continue;
    const { meta } = answers(table, "csv");
    createRaw(db, table);
    const selected = meta.columns.map(
      (column, at) => `${typed(column, `f${at}`)} AS ${name(column.name)}`,
    );
    db.run(
      `COPY (SELECT ${selected.join(", ")} FROM raw ORDER BY rn) TO ${literal(`${stem(table)}.parquet`)} ` +
        `(${copyOptions(table.parquet)})`,
    );
    dropAll(db);
  }
  list.parquetOnly = [];
  for (const [about, select] of Object.entries(PARQUET_ONLY)) {
    for (const [number, rows] of [0, 1, 3_000, 70_000].entries()) {
      const table = { name: `${about} ${rows}`, about: "types that only Parquet has", rows };
      const options = {
        compression: ["zstd", "snappy", "gzip", "lz4_raw"][number],
        rowGroup: [2048, 2048, 2048, 30_000][number],
        version: ["V1", "V2", "V2", "V1"][number],
      };
      db.run(
        `COPY (${select.replace("ROWS", String(rows))}) TO ${literal(`${stem(table)}.parquet`)} (${copyOptions(options)})`,
      );
      list.parquetOnly.push({ ...table, parquet: options });
    }
  }
  writeFileSync(listPath, `${JSON.stringify(list, null, 2)}\n`);
}

/** The value of a column of a Parquet file as a table has it. */
function fromParquet(column, type) {
  const c = name(column.name);
  const { type: ours, scale } = column.kind;
  const refuse = () => {
    throw new Error(`${column.name}: ${type} became ${JSON.stringify(column.kind)}`);
  };
  switch (true) {
    case /^U?(TINY|SMALL|BIG)?INT(EGER)?$/.test(type):
      if (ours === "integer") return `CAST(${c} AS BIGINT)`;
      // Numbers above the largest signed one make the column text.
      return ours === "text" && type === "UBIGINT" ? `CAST(${c} AS VARCHAR)` : refuse();
    case type.startsWith("DECIMAL"): {
      const [digits, places] = type.match(/\d+/g).map(Number);
      if (digits <= 18) return ours === "decimal" && scale === places ? c : refuse();
      return ours === "text" ? `CAST(${c} AS VARCHAR)` : refuse();
    }
    case type === "FLOAT" || type === "DOUBLE":
      return ours === "float" ? `CASE WHEN isfinite(${c}) THEN CAST(${c} AS DOUBLE) END` : refuse();
    case type === "DATE":
      return ours === "date" ? c : refuse();
    case type === "TIMESTAMP_NS":
      // Parts of a microsecond are cut off towards the past.
      return ours === "datetime"
        ? `make_timestamp((epoch_ns(${c}) - (epoch_ns(${c}) % 1000 + 1000) % 1000) // 1000)`
        : refuse();
    case type.startsWith("TIMESTAMP"):
      return ours === "datetime" ? `make_timestamp(epoch_us(${c}))` : refuse();
    case type === "BOOLEAN":
      return ours === "boolean" ? c : refuse();
    case type === "TIME": {
      // With three or six decimals of a second, or none.
      const text = `CAST(${c} AS VARCHAR)`;
      const padded = `rpad(${text}, CASE WHEN length(${text}) <= 12 THEN 12 ELSE 15 END, '0')`;
      return ours === "text"
        ? `CASE WHEN contains(${text}, '.') THEN ${padded} ELSE ${text} END`
        : refuse();
    }
    case type === "UUID":
      return ours === "text" ? `CAST(${c} AS VARCHAR)` : refuse();
    case type === "BLOB":
      return ours === "text" ? `nullif(decode(${c}), '')` : refuse();
    case type === "VARCHAR" || type.startsWith("ENUM"):
      return ours === "text" ? `nullif(CAST(${c} AS VARCHAR), '')` : refuse();
    default:
      return refuse();
  }
}

// Comparing.

const counts = { cells: 0, types: 0, view: 0, summary: 0, values: 0, parquetCounts: 0 };
const allowed = new Map();
const mismatches = [];

const allow = (reason, times = 1) => allowed.set(reason, (allowed.get(reason) ?? 0) + times);

function differ(what, details) {
  mismatches.push({ ...what, ...details });
  const text = JSON.stringify({ ...what, ...details }, (_, value) =>
    typeof value === "bigint" ? String(value) : value,
  );
  console.log(`DIFFERENT ${text.slice(0, 1_500)}`);
}

function compareTypes(db, table, columns, what) {
  const found = db.rows(
    `DESCRIBE SELECT * FROM read_csv(${literal(table.csv)}, ${csvOptions(table)}, sample_size = -1)`,
  );
  const same = {
    integer: "BIGINT",
    float: "DOUBLE",
    date: "DATE",
    datetime: "TIMESTAMP",
    boolean: "BOOLEAN",
    text: "VARCHAR",
  };
  for (const [at, column] of columns.entries()) {
    counts.types++;
    const ours = column.kind.type;
    const theirs = found[at].column_type;
    if (found[at].column_name !== column.name) {
      differ(what, { column: at, ours: column.name, theirs: found[at].column_name });
    }
    // Numbers without an exponent that a decimal holds do not become floats.
    const loses = (of) =>
      db
        .values(`SELECT DISTINCT f${of} FROM raw WHERE f${of} IS NOT NULL`)[0]
        .find((text) => /^-?\d+(\.\d+)?([eE][+-]?\d+)?$/.test(text) && !comesBack(text));
    if (ours === "float" && loses(at) !== undefined) {
      differ(what, { column: at, name: column.name, ours, loses: loses(at) });
    }
    if (same[ours] === theirs) continue;
    const holds = (condition, of) =>
      Number(
        db.row(`SELECT count(*) AS n FROM (SELECT f${of} AS c FROM raw) WHERE ${condition}`).n,
      ) > 0;
    const known = TYPE_DIFFERENCES.find(
      (one) =>
        one.ours.includes(ours) &&
        one.theirs.includes(theirs) &&
        (one.holds === undefined || holds(one.holds, at)) &&
        (!one.loses || loses(at) !== undefined) &&
        (one.anywhere === undefined || columns.some((_, of) => holds(one.anywhere, of))),
    );
    if (known === undefined) differ(what, { column: at, name: column.name, ours, theirs });
    else allow(known.reason);
  }
}

function compareCells(db, meta, bytes, what) {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const { rows } = meta;
  for (const [at, column] of meta.columns.entries()) {
    counts.cells++;
    const { type } = column.kind;
    const start = column.cells.at;
    const numbers = start + Math.ceil(rows / 8) * 8;
    const texts = numbers + rows * 8;
    const of = type === "text" || type === "float" ? `c${at}` : `i${at}`;
    const [found] = db.values(`SELECT ${of} FROM t ORDER BY rn`);
    if (found.length !== rows) {
      differ(what, { column: at, rows, theirs: found.length });
      continue;
    }
    let from = 0;
    let blanks = 0;
    let same = true;
    for (let row = 0; row < rows; row++) {
      let ours = null;
      const blank = bytes[start + row] === 1;
      if (type === "text") {
        const end = Number(view.getBigUint64(numbers + row * 8, true));
        if (!blank) ours = bytes.toString("utf8", texts + from, texts + end);
        from = end;
      } else if (!blank) {
        ours =
          type === "float"
            ? view.getFloat64(numbers + row * 8, true)
            : view.getBigInt64(numbers + row * 8, true);
      }
      if (blank) blanks++;
      // There is one zero.
      const theirs = found[row] === 0 ? 0 : found[row];
      if (Object.is(ours, theirs)) continue;
      differ(what, { column: at, name: column.name, row, ours, theirs });
      same = false;
      break;
    }
    if (same && blanks !== column.blanks) differ(what, { column: at, blanks, ours: column.blanks });
  }
}

function compareView(db, table, meta, bytes, question, what) {
  const sql = `SELECT CAST(rn AS INTEGER) FROM t ${where(meta.columns, question.filters)} ${order(meta.columns, question.order)}`;
  if (question.refused !== undefined) {
    if (table.mayRefuse && question.refused === "TooManyEqualPrefixes")
      return allow(REFUSED.reason);
    return differ(what, { refused: question.refused, sql });
  }
  const [theirs] = db.columns(sql);
  const ours = new Uint32Array(question.len);
  for (let at = 0; at < question.len; at++) ours[at] = bytes.readUInt32LE(question.at + at * 4);
  const first = ours.findIndex((row, at) => row !== theirs[at]);
  if (first >= 0 || ours.length !== theirs.length) {
    differ(what, {
      rows: [ours.length, theirs.length],
      first,
      ours: [...ours.slice(first, first + 5)],
      theirs: [...theirs.slice(first, first + 5)],
      sql,
    });
  }
}

const isCoded = (column) => column.dictionary && column.plainBlocks === 0;

function compareSummary(db, meta, question, what) {
  const filter = where(meta.columns, question.filters);
  const and = (more) => (filter === "" ? `WHERE ${more}` : `${filter} AND ${more}`);
  for (const ours of question.columns) {
    const at = ours.column;
    const column = meta.columns[at];
    const { kind } = column;
    const selected = ["count(*) AS n", `count(c${at}) AS filled`];
    if (kind.type === "text") {
      selected.push(
        `count(DISTINCT l${at}) AS distinct_texts`,
        `min(l${at}) AS low`,
        `max(l${at}) AS high`,
      );
    } else if (kind.type === "float") {
      selected.push(`min(c${at}) AS low`, `max(c${at}) AS high`, `fsum(c${at}) AS total`);
    } else {
      selected.push(
        `CAST(min(i${at}) AS VARCHAR) AS low`,
        `CAST(max(i${at}) AS VARCHAR) AS high`,
        `CAST(sum(CAST(i${at} AS HUGEINT)) AS VARCHAR) AS total`,
      );
    }
    const sql = `SELECT ${selected.join(", ")} FROM t ${filter}`;
    const theirs = db.row(sql);
    const rows = Number(theirs.n);
    const filled = Number(theirs.filled);
    const wrong = (of, value) =>
      differ(what, { column: at, of, ours: ours[of], theirs: value, sql });
    if (ours.rows !== rows) wrong("rows", rows);
    if (ours.blanks !== rows - filled) wrong("blanks", rows - filled);
    const isNumber = ["integer", "decimal", "float"].includes(kind.type);
    if (!isNumber || filled === 0) {
      if (ours.sum !== null) wrong("sum", null);
      if (ours.mean !== null) wrong("mean", null);
    }
    if (kind.type === "text") {
      const known = isCoded(column) || column.blanks === meta.rows;
      const distinct = known ? Number(theirs.distinct_texts) : null;
      if (ours.distinct !== distinct) wrong("distinct", distinct);
      for (const [of, text] of [
        ["min", theirs.low],
        ["max", theirs.high],
      ]) {
        // Spelled like the first one in the file, whether it is in the view or not.
        const spelled =
          distinct === null || filled === 0
            ? null
            : db.row(`SELECT arg_min(c${at}, rn) AS c FROM t WHERE l${at} = ${literal(text)}`).c;
        if ((ours[of]?.text ?? null) !== spelled) wrong(of, spelled);
      }
      continue;
    }
    if (ours.distinct !== null) wrong("distinct", null);
    if (filled === 0) {
      if (ours.min !== null) wrong("min", null);
      if (ours.max !== null) wrong("max", null);
    } else if (kind.type === "float") {
      if (!isShown(ours.min?.text ?? "", theirs.low)) wrong("min", theirs.low);
      if (!isShown(ours.max?.text ?? "", theirs.high)) wrong("max", theirs.high);
      const [numbers] = db.columns(`SELECT c${at} FROM t ${and(`c${at} IS NOT NULL`)} ORDER BY rn`);
      const total = exactSum(numbers);
      if (!isSum(ours.sum?.text ?? "0", total, filled, 1n)) wrong("sum", theirs.total);
      if (!isSum(ours.mean?.text ?? "0", total, filled, BigInt(filled)))
        wrong("mean", theirs.total / filled);
      if (!isShown(ours.sum?.text ?? "", theirs.total)) allow(FLOAT_SUM.reason);
    } else {
      const text = (value) => shown(kind, BigInt(value));
      if (ours.min?.text !== text(theirs.low)) wrong("min", text(theirs.low));
      if (ours.max?.text !== text(theirs.high)) wrong("max", text(theirs.high));
      if (isNumber) {
        const sum = text(theirs.total);
        if (ours.sum?.text !== sum || !ours.sum.exact) wrong("sum", sum);
        if (
          ours.mean === null ||
          !isMean(ours.mean, BigInt(theirs.total), filled, kind.scale ?? 0)
        ) {
          wrong("mean", `${sum} / ${filled}`);
        }
      }
    }
  }
}

function compareValues(db, meta, question, what) {
  const at = question.column;
  const { limit } = question;
  const column = meta.columns[at];
  const { kind } = column;
  const search = question.search.toLowerCase();
  const others = question.filters.filter(([of]) => of !== at);
  const filter = where(meta.columns, others);
  const and = (more) => (filter === "" ? `WHERE ${more}` : `${filter} AND ${more}`);
  const { n } = db.row(`SELECT count(*) AS n FROM t ${and(`c${at} IS NULL`)}`);
  if (Number(n) !== question.blanks)
    differ(what, { blanks: Number(n), ours: question.blanks, filter });

  let expected;
  let all;
  if (kind.type === "text") {
    // Texts are kept folded, and also as they are spelled when that differs.
    const sizes = db.row(
      "SELECT count(*) AS n, sum(strlen(l) + CASE WHEN s = l THEN 0 ELSE strlen(s) END) AS bytes " +
        `FROM (SELECT l${at} AS l, arg_min(c${at}, rn) AS s FROM t ${and(`l${at} IS NOT NULL`)} GROUP BY ALL)`,
    );
    if (
      !isCoded(column) &&
      (Number(sizes.n) > LISTED.values || Number(sizes.bytes ?? 0) > LISTED.bytes)
    ) {
      allow(LISTED.reason);
      if (question.complete) differ(what, { complete: true, distinct: Number(sizes.n) });
      for (const [value, , count] of question.values) {
        const found = db.row(
          `SELECT count(*) AS n FROM t ${and(`l${at} = ${literal(value.text.toLowerCase())}`)}`,
        );
        if (Number(found.n) !== count) differ(what, { value, count, theirs: Number(found.n) });
      }
      return;
    }
    const part = search === "" ? "" : ` AND contains(l${at}, ${literal(search)})`;
    const rows = db.rows(
      `SELECT l${at} AS l, count(*) AS n, arg_min(c${at}, rn) AS s FROM t ${and(`l${at} IS NOT NULL${part}`)} ` +
        `GROUP BY ALL ORDER BY n DESC, l LIMIT ${limit + 1}`,
    );
    all = rows.length;
    const best = rows.slice(0, limit).sort((a, b) => compareBytes(a.l, b.l));
    if (isCoded(column) && best.length > 0) {
      // Spelled like the first one of the file, whether it is counted or not.
      const first = new Map(
        db
          .rows(
            `SELECT l${at} AS l, arg_min(c${at}, rn) AS s FROM t WHERE l${at} IN (${best.map((row) => literal(row.l)).join(", ")}) GROUP BY ALL`,
          )
          .map((row) => [row.l, row.s]),
      );
      for (const row of best) row.s = first.get(row.l);
    }
    const partly = column.dictionary && column.plainBlocks > 0;
    expected = best.map((row) => {
      // Of texts that are partly in a dictionary, its entries come first.
      const ours = question.values.find(([value]) => value.text?.toLowerCase() === row.l);
      const text = partly && ours !== undefined ? ours[0].text : row.s;
      return [{ text }, text, Number(row.n)];
    });
  } else {
    const of = kind.type === "float" ? `c${at}` : `i${at}`;
    const [keys, times] = db.columns(
      `SELECT ${of}, count(*) FROM t ${and(`c${at} IS NOT NULL`)} GROUP BY ALL`,
    );
    let found = Array.from(keys, (key, index) => ({ key, n: Number(times[index]) }));
    if (found.length > LISTED.values) throw new Error("too many values to be compared");
    if (search !== "") {
      found = found.filter(({ key }) => shown(kind, key).toLowerCase().includes(search));
    }
    all = found.length;
    const byKey = (a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0);
    found.sort((a, b) => b.n - a.n || byKey(a, b));
    expected = found
      .slice(0, limit)
      .sort(byKey)
      .map(({ key, n: count }) => {
        if (kind.type !== "float") return [{ int: String(key) }, shown(kind, key), count];
        const bits = new DataView(new ArrayBuffer(8));
        bits.setFloat64(0, key === 0 ? 0 : key);
        const float = bits.getBigUint64(0).toString(16).padStart(16, "0");
        const text = question.values.find(([value]) => value.float === float)?.[1] ?? "";
        return [{ float }, isShown(text, key) ? text : String(key), count];
      });
  }
  const complete = all <= limit;
  const ours = JSON.stringify(question.values);
  if (ours !== JSON.stringify(expected) || complete !== question.complete) {
    const first = expected.findIndex(
      (one, index) => JSON.stringify(one) !== JSON.stringify(question.values[index]),
    );
    differ(what, {
      column: at,
      search: question.search,
      limit,
      complete: [question.complete, complete],
      lengths: [question.values.length, expected.length],
      first,
      ours: question.values[first],
      theirs: expected[first],
      filter,
    });
  }
}

/** What the import of a Parquet file says it changed. */
function compareCounts(db, meta, types, what) {
  for (const [at, column] of meta.columns.entries()) {
    const type = types.get(column.name);
    const c = name(column.name);
    const ours = meta.report.counts[at];
    const count = (condition) => Number(db.row(`SELECT count(*) AS n FROM p WHERE ${condition}`).n);
    const isFloat = type === "FLOAT" || type === "DOUBLE";
    const theirs = {
      cutNanos: type === "TIMESTAMP_NS" ? count(`epoch_ns(${c}) % 1000 <> 0`) : 0,
      notFinite: isFloat ? count(`${c} IS NOT NULL AND NOT isfinite(${c})`) : 0,
      outOfRange: 0,
    };
    counts.parquetCounts++;
    if (JSON.stringify(theirs) !== JSON.stringify(ours)) differ(what, { column: at, ours, theirs });
  }
}

function compareTable(db, table, from) {
  const { meta, questions, bytes } = answers(table, from);
  const what = { table: table.name, from };
  if (from === "csv") {
    createRaw(db, table);
    compareTypes(db, table, meta.columns, { ...what, what: "types" });
    const folded = createTable(db, meta.columns, (column, at) => typed(column, `f${at}`), "raw");
    if (folded > 0) allow(FOLDING.reason, folded);
  } else {
    const source = `read_parquet(${literal(`${stem(table)}.parquet`)}, file_row_number = true)`;
    db.run(`CREATE VIEW p AS SELECT * FROM ${source}`);
    const types = new Map(
      db.rows("DESCRIBE SELECT * FROM p").map((row) => [row.column_name, row.column_type]),
    );
    const nested = [...types]
      .filter(([, type]) => /\[\]$|^STRUCT|^MAP/.test(type))
      .map(([of]) => of);
    const left = [...types.keys()].filter(
      (of) => of !== "file_row_number" && !meta.columns.some((column) => column.name === of),
    );
    if (left.join() !== nested.join() || meta.report.skipped.join() !== nested.join()) {
      differ(what, { skipped: meta.report.skipped, theirs: nested, left });
    }
    compareCounts(db, meta, types, { ...what, what: "counts" });
    const folded = createTable(
      db,
      meta.columns,
      (column) => fromParquet(column, types.get(column.name)),
      "(SELECT *, file_row_number AS rn FROM p)",
    );
    if (folded > 0) allow(FOLDING.reason, folded);
    db.run("DROP VIEW p");
  }
  compareCells(db, meta, bytes, { ...what, what: "cells" });
  for (const [number, question] of questions.entries()) {
    const of = { ...what, what: question.what, question: number };
    counts[question.what]++;
    const compare = {
      view: () => compareView(db, table, meta, bytes, question, of),
      summary: () => compareSummary(db, meta, question, of),
      values: () => compareValues(db, meta, question, of),
    };
    try {
      compare[question.what]();
    } catch (error) {
      throw new Error(`${JSON.stringify(of)} ${JSON.stringify(question).slice(0, 2_000)}`, {
        cause: error,
      });
    }
  }
  dropAll(db);
  const dictionaries = meta.columns.filter((column) => column.dictionary);
  return {
    name: table.name,
    from,
    about: table.about,
    rows: meta.rows,
    groups: meta.groups,
    columns: meta.columns.length,
    questions: questions.length,
    ...(from === "csv" ? { passes: meta.report.passes } : { parquet: table.parquet }),
    dictionaries: dictionaries.length,
    dictionariesLeft: dictionaries.filter((column) => column.plainBlocks > 0).length,
  };
}

function compareAll(db) {
  const tables = [];
  for (const table of list.tables) tables.push(compareTable(db, table, "csv"));
  for (const table of [...list.tables, ...list.parquetOnly]) {
    if (table.parquet !== null) tables.push(compareTable(db, table, "parquet"));
  }
  const comparisons = counts.view + counts.summary + counts.values;
  const report = {
    reference: `DuckDB ${DUCKDB_VERSION} (duckdb-wasm 1.32.0)`,
    seed: list.seed,
    comparisons,
    byKind: counts,
    allowed: Object.fromEntries([...allowed].sort()),
    mismatches: mismatches.length,
    tables,
  };
  writeFileSync(join(dir, "report.json"), `${JSON.stringify(report, null, 2)}\n`);
  console.log(`${tables.length} tables, ${comparisons} questions, ${mismatches.length} different`);
  for (const [reason, times] of allowed) console.log(`  ${times} times: ${reason}`);
  if (mismatches.length > 0) process.exitCode = 1;
}

try {
  const db = await open();
  if (stage === "parquet") writeParquet(db);
  else compareAll(db);
} catch (error) {
  // The source around it, which is printed otherwise, is one line of a million characters.
  console.error(error.message, error.cause?.message ?? "");
  console.error(
    error.stack
      .split("\n")
      .filter((line) => !line.includes("duckdb-node"))
      .join("\n")
      .slice(0, 3_000),
  );
  process.exitCode = 1;
}