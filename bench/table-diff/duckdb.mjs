// DuckDB-WASM in this process, of the version package.json pins, with the
// Parquet extension that setup.mjs put into place.

import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { homedir } from "node:os";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const tools = join(here, "../../.tools");

export const MODULES = join(tools, "duckdb");
export const HOME = join(tools, "duckdb-home");

/** The release of DuckDB in the package, and its extension for Parquet. */
export const DUCKDB_VERSION = "v1.4.3";
export const EXTENSION_URL = `https://extensions.duckdb.org/${DUCKDB_VERSION}/wasm_eh/parquet.duckdb_extension.wasm`;
export const EXTENSION_SHA256 = "22765c8f7dc741cda2b571a66ac7bb355295d7d69a6c37e5315b265672984f55";

/** Where DuckDB looks for an extension in the home directory, and downloads it to when it is not there. */
export const EXTENSION = join(
  HOME,
  ".duckdb/extensions/extensions.duckdb.org",
  DUCKDB_VERSION,
  "wasm_eh/parquet.duckdb_extension.wasm",
);

/** Whether the extension is there and is the one that is meant. */
export function hasExtension() {
  if (!existsSync(EXTENSION)) return false;
  return createHash("sha256").update(readFileSync(EXTENSION)).digest("hex") === EXTENSION_SHA256;
}

/** A connection to a database in memory. Files are found in the working directory. */
export async function open() {
  if (relative(HOME, homedir()) !== "") throw new Error(`the home directory is ${homedir()}`);
  if (!hasExtension()) throw new Error("the Parquet extension is missing: run setup.mjs");
  const dist = join(MODULES, "node_modules/@duckdb/duckdb-wasm/dist");
  const duckdb = createRequire(import.meta.url)(join(dist, "duckdb-node-blocking.cjs"));
  const bundle = (name) => ({ mainModule: join(dist, `duckdb-${name}.wasm`), mainWorker: null });
  const db = await duckdb.createDuckDB(
    { mvp: bundle("mvp"), eh: bundle("eh") },
    new duckdb.VoidLogger(),
    duckdb.NODE_RUNTIME,
  );
  await db.instantiate();
  if (db.getVersion() !== DUCKDB_VERSION) throw new Error(`DuckDB is ${db.getVersion()}`);
  db.open({ path: ":memory:" });
  const connection = db.connect();
  connection.query("SET autoinstall_known_extensions = false");
  connection.query("SET preserve_insertion_order = true");
  connection.query("LOAD parquet");
  return new Connection(connection);
}

class Connection {
  #connection;

  constructor(connection) {
    this.#connection = connection;
  }

  run(sql) {
    this.#connection.query(sql);
  }

  /** The rows of an answer, as objects. */
  rows(sql) {
    return this.#connection
      .query(sql)
      .toArray()
      .map((row) => row.toJSON());
  }

  /** The one row of an answer. */
  row(sql) {
    const rows = this.rows(sql);
    if (rows.length !== 1) throw new Error(`${rows.length} rows: ${sql}`);
    return rows[0];
  }

  /** The columns of an answer, which are numbers without blanks, as typed arrays. */
  columns(sql) {
    const table = this.#connection.query(sql);
    return table.schema.fields.map((_, index) => {
      const column = table.getChildAt(index);
      if (column.nullCount > 0) throw new Error(`blanks: ${sql}`);
      return column.toArray();
    });
  }

  /** The columns of an answer, one value after another. */
  values(sql) {
    const table = this.#connection.query(sql);
    return table.schema.fields.map((_, index) => [...table.getChildAt(index)]);
  }
}

export const literal = (text) => `'${text.replaceAll("'", "''")}'`;
export const name = (text) => `"${text.replaceAll('"', '""')}"`;