// Puts the Parquet extension of DuckDB into place: downloads it once, and
// makes sure that it is the one whose SHA-256 is written down.

import { createHash } from "node:crypto";
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import { EXTENSION, EXTENSION_SHA256, EXTENSION_URL, hasExtension } from "./duckdb.mjs";

if (!hasExtension()) {
  console.log(`downloading ${EXTENSION_URL}`);
  const response = await fetch(EXTENSION_URL);
  if (!response.ok) throw new Error(`${EXTENSION_URL}: ${response.status}`);
  const bytes = new Uint8Array(await response.arrayBuffer());
  const found = createHash("sha256").update(bytes).digest("hex");
  if (found !== EXTENSION_SHA256) {
    throw new Error(`${EXTENSION_URL} is ${found}, expected ${EXTENSION_SHA256}`);
  }
  mkdirSync(dirname(EXTENSION), { recursive: true });
  writeFileSync(EXTENSION, bytes);
}
console.log(`Parquet extension ${EXTENSION_SHA256.slice(0, 12)} is in place`);