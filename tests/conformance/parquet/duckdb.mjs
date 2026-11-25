// Writes the Parquet files of this directory that DuckDB writes, and the
// values of those that Arrow writes.
//
//   node duckdb.mjs <dist of @duckdb/duckdb-wasm> write <directory>
//   node duckdb.mjs <dist of @duckdb/duckdb-wasm> read <directory> <name>...

import { readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { join } from "node:path";

const [dist, command, directory, ...names] = process.argv.slice(2);
const duckdb = createRequire(import.meta.url)(join(dist, "duckdb-node-blocking.cjs"));
const bundle = (name) => ({ mainModule: join(dist, `duckdb-${name}.wasm`) });
const db = await duckdb.createDuckDB(
  { mvp: bundle("mvp"), eh: bundle("eh") },
  new duckdb.VoidLogger(),
  duckdb.NODE_RUNTIME,
);
await db.instantiate();
db.open({});
const connection = db.connect();
const rows = (sql) => connection.query(sql).toArray();

const values = {
  truth: "n % 3 = 0",
  i8: "(n - 30)::TINYINT",
  i16: "((n - 30) * 1000)::SMALLINT",
  i32: "((n - 30) * 70000000)::INTEGER",
  i64: "((n - 30)::BIGINT * 300000000000000000)::BIGINT",
  u8: "(n * 4)::UTINYINT",
  u16: "(n * 1000)::USMALLINT",
  u32: "(n * 70000000)::UINTEGER",
  u64: "(n::UBIGINT * 100000000000000000)::UBIGINT",
  f32: "(n / 10)::FLOAT",
  f64: "CASE n WHEN 0 THEN 'nan' WHEN 1 THEN '-inf' WHEN 2 THEN '-0.0' ELSE n / 7 END::DOUBLE",
  dec4: "((n - 30) / 4)::DECIMAL(4, 1)",
  dec9: "((n - 30) / 8)::DECIMAL(9, 3)",
  dec18: "((n - 30) * 12345678.5)::DECIMAL(18, 6)",
  dec38: "((n - 30)::DECIMAL(38, 10) * 100000000000000000000)::DECIMAL(38, 10)",
  day: "DATE '0001-01-01' + (n * 60000)::INTEGER",
  s: "('1970-01-01'::TIMESTAMP + INTERVAL (n * 99999) SECOND)::TIMESTAMP_S",
  ms: "('1970-01-01'::TIMESTAMP + INTERVAL ((n - 30) * 99999) MILLISECOND)::TIMESTAMP_MS",
  us: "'0001-01-01'::TIMESTAMP + INTERVAL (n * 59999) DAY + INTERVAL (n) MICROSECOND",
  ns: "make_timestamp_ns((n - 30)::BIGINT * 100000000000000 + n % 4 * 250)",
  utc: "make_timestamptz((n - 30)::BIGINT * 100000000000001)",
  time: "TIME '00:00:00' + INTERVAL (n * 1400) SECOND + INTERVAL (n % 3) MICROSECOND",
  text: "repeat(['', 'a' || chr(9) || 'b', 'c' || chr(10) || chr(92), '漢字', '😀', 'plain'][n % 6 + 1], (n % 4)::INTEGER)",
  many: "'value ' || n",
  choice: "['north', 'south', 'east'][n % 3 + 1]::ENUM ('east', 'north', 'south')",
  id: "printf('%08x-0000-4000-8000-%012x', n * 1000003, n)::UUID",
};

function write() {
  const missing = "n % 7 = 3 OR n BETWEEN 40 AND 49";
  const columns = Object.entries(values).map(
    ([name, sql]) => `CASE WHEN ${missing} THEN NULL ELSE ${sql} END AS "${name}"`,
  );
  const copy = (name, sql, options) => {
    // A file that is there already is not written over.
    rmSync(name, { force: true });
    connection.query(`COPY (${sql}) TO '${name}' (FORMAT parquet, USE_TMP_FILE false, ${options})`);
    writeFileSync(join(directory, `${name}.parquet`), db.copyFileToBuffer(name));
    rmSync(name, { force: true });
  };
  connection.query(
    `CREATE TABLE t AS SELECT ${columns.join(", ")}, n::INTEGER AS must FROM range(61) r(n)`,
  );
  for (const codec of ["snappy", "gzip", "zstd", "lz4_raw"]) {
    copy(`duckdb-${codec}`, "FROM t", `COMPRESSION ${codec}, PARQUET_VERSION v1`);
  }
  copy("duckdb-v2", "FROM t", "COMPRESSION uncompressed, PARQUET_VERSION v2, ROW_GROUP_SIZE 2048");
  copy(
    "duckdb-others",
    `SELECT n AS before, INTERVAL (n) DAY AS gap, '\\xFFb'::BLOB AS bytes,
       [n, NULL] AS list, {a: n} AS struct, MAP {'k': n} AS map, 'x' AS after FROM range(9) r(n)`,
    "COMPRESSION snappy",
  );
}

const threes = (digits) => {
  let rest = digits.padEnd(9, "0");
  while (rest.endsWith("000")) rest = rest.slice(0, -3);
  return rest ? `.${rest}` : "";
};

/** A moment in microseconds as the tests write it; blank outside the years 1 to 9999. */
function moment(micros) {
  const day = 86_400_000_000n;
  let days = micros / day;
  let rest = micros % day;
  if (rest < 0n) {
    rest += day;
    days -= 1n;
  }
  if (days < -719_162n || days > 2_932_896n) return "";
  const date = new Date(Number(days) * 86_400_000).toISOString().slice(0, 10);
  const seconds = Number(rest / 1_000_000n);
  const two = (n) => String(n).padStart(2, "0");
  const time = [Math.floor(seconds / 3600), Math.floor(seconds / 60) % 60, seconds % 60]
    .map(two)
    .join(":");
  return `${date} ${time}${threes(String(rest % 1_000_000n).padStart(6, "0"))}`;
}

const escapes = { "\\": "\\\\", "\t": "\\t", "\n": "\\n", "\r": "\\r" };

function read(name) {
  db.registerFileBuffer(`${name}.parquet`, readFileSync(join(directory, `${name}.parquet`)));
  const from = `FROM '${name}.parquet'`;
  const header = [];
  const columns = [];
  for (const { column_name: column, column_type: type } of rows(`DESCRIBE SELECT * ${from}`)) {
    if (/\[\]$|^(STRUCT|MAP|INTERVAL)/.test(type)) continue;
    const quoted = `"${column}"`;
    let sql = `${quoted}::VARCHAR`;
    let show = (text) => text.replace(/[\\\t\n\r]/g, (found) => escapes[found]);
    if (/^TIMESTAMP_NS/.test(type)) {
      sql = `epoch_ns(${quoted})::VARCHAR`;
      show = (text) => {
        const nanos = BigInt(text);
        const micros = nanos / 1000n;
        return moment(nanos < 0n && nanos % 1000n !== 0n ? micros - 1n : micros);
      };
    } else if (/^TIMESTAMP/.test(type)) {
      sql = `epoch_us(${quoted})::VARCHAR`;
      show = (text) => moment(BigInt(text));
    } else if (/^TIME/.test(type)) {
      show = (text) => {
        const [whole, rest = ""] = text.split(".");
        return whole + threes(rest);
      };
    } else if (type === "DATE") {
      show = (text) => (/^\d{4}-\d\d-\d\d$/.test(text) ? text : "");
    } else if (type === "BOOLEAN") {
      show = (text) => text.toUpperCase();
    } else if (type === "FLOAT" || type === "DOUBLE") {
      sql = `${quoted}::DOUBLE::VARCHAR`;
      show = (text) => (/nan|inf/.test(text) ? "" : text);
    } else if (type === "BLOB") {
      sql = `decode(${quoted})`;
    }
    let cells;
    try {
      cells = rows(`SELECT ${sql} AS v ${from}`).map(({ v }) =>
        v === null ? "" : show(String(v)),
      );
    } catch (error) {
      // Bytes that are not text.
      if (type !== "BLOB") throw error;
      continue;
    }
    header.push(column);
    columns.push(cells);
  }
  const lines = [header, ...(columns[0] ?? []).map((_, row) => columns.map((cells) => cells[row]))];
  writeFileSync(
    join(directory, `${name}.tsv`),
    lines.map((line) => `${line.join("\t")}\n`).join(""),
  );
}

if (command === "write") write();
else for (const name of names) read(name);
