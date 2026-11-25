// Writes the files that the measurements of data tables read.
//
//   node bench/table-data.mjs orders <rows> <file>
//   node bench/table-data.mjs hard <rows> <file>
//
// The same rows every time: everything comes from one seeded generator.

import { closeSync, createReadStream, createWriteStream, openSync, readSync, statSync, writeSync } from "node:fs";
import { resolve } from "node:path";
import { pipeline } from "node:stream/promises";
import { fileURLToPath } from "node:url";

/** 32 random bits at a time (xorshift32). */
function generator() {
  let state = 0x9e3779b9;
  return () => {
    state ^= state << 13;
    state ^= state >>> 17;
    state ^= state << 5;
    return state >>> 0;
  };
}

function writeRows(path, header, rows, row) {
  const fd = openSync(path, "w");
  writeSync(fd, `${header}\n`);
  let chunk = [];
  for (let id = 1; id <= rows; id++) {
    chunk.push(row(id));
    if (chunk.length === 20_000) {
      writeSync(fd, chunk.join(""));
      chunk = [];
    }
  }
  writeSync(fd, chunk.join(""));
  closeSync(fd);
  return statSync(path).size;
}

const REGIONS = ["North", "South", "East", "West", "Central", "Harbour", "Airport", "Online"];
const WORDS = ["late", "gift", "return", "exchange", "fragile", "priority", "repeat", "bulk", "online", "counter"];
const START = Date.UTC(2023, 0, 1);

/** The columns of the orders, by what is asked of them. */
export const COLUMNS = {
  id: 0,
  orderedAt: 1,
  store: 3,
  region: 4,
  product: 5,
  customer: 6,
  quantity: 7,
  amount: 10,
  note: 12,
};

/** Made-up orders: thirteen columns of all types. Gives the bytes of the file. */
export function writeOrders(path, rows) {
  const bits = generator();
  const below = (limit) => bits() % limit;
  const two = (value) => String(value).padStart(2, "0");
  const money = (value) => `${Math.trunc(value / 100)}.${two(value % 100)}`;
  const products = Array.from({ length: 2000 }, (_, index) => `P-${String(index).padStart(4, "0")} ${WORDS[index % WORDS.length]}`);
  let at = START;
  return writeRows(path, "id,ordered_at,day,store,region,product,customer,quantity,unit_price,discount,amount,paid,note", rows, (id) => {
    // About ten million orders in two years and a half, in order of time.
    at += below(16000);
    const date = new Date(at);
    const day = `${date.getUTCFullYear()}-${two(date.getUTCMonth() + 1)}-${two(date.getUTCDate())}`;
    const time = `${two(date.getUTCHours())}:${two(date.getUTCMinutes())}:${two(date.getUTCSeconds())}`;
    const quantity = 1 + below(12);
    const cents = 99 + below(50000);
    const discount = below(5) === 0 ? below(40) : 0;
    const amount = Math.round((quantity * cents * (100 - discount)) / 100);
    const note = below(10) === 0 ? (below(3) === 0 ? `"${WORDS[below(10)]}, ${WORDS[below(10)]}"` : `${WORDS[below(10)]} ${below(1000)}`) : "";
    return `${id},${day} ${time},${day},${below(5000)},${REGIONS[below(8)]},${products[below(2000)]},C${String(below(1200000)).padStart(7, "0")},${quantity},${money(cents)},${discount === 0 ? "" : (discount / 100).toFixed(2)},${money(amount)},${below(20) === 0 ? "false" : "true"},${note}\n`;
  });
}

/** Rows that do not compress: texts of 36 bytes that are all different, and numbers of 16 to 17 digits. */
export function writeHard(path, rows) {
  const bits = generator();
  const hex = (value, digits) => value.toString(16).padStart(digits, "0");
  const number = () => ((bits() * 2 ** 21 + (bits() >>> 11)) / 2 ** 53 - 0.5) * 1e6;
  return writeRows(path, "id,key,value,other", rows, (id) => {
    const key = `${hex(bits(), 8)}-${hex(bits() >>> 16, 4)}-${hex(bits() >>> 16, 4)}-${hex(bits() >>> 16, 4)}-${hex(id, 4).slice(-4)}${hex(bits(), 8)}`;
    return `${id},${key},${number()},${number()}\n`;
  });
}

/**
 * Copies orders, and has the first row that begins at an offset order half of
 * something: the column is one of whole numbers until then. Gives the bytes of
 * the file, and where the surprise is.
 */
export async function copyWithSurprise(from, to, offset) {
  const { size } = statSync(from);
  const fd = openSync(from, "r");
  // Rows are far shorter than this.
  const window = Buffer.alloc(4096);
  const start = Math.max(0, Math.min(offset, size - 2048));
  const length = readSync(fd, window, 0, window.length, start);
  closeSync(fd);
  const text = window.toString("latin1", 0, length);
  const isLast = offset >= size - 2048;
  const line = isLast ? text.trimEnd().lastIndexOf("\n") + 1 : text.indexOf("\n") + 1;
  // The product and the customer, of which there are no commas in either, and the quantity.
  const [found, before] = /^((?:[^,]*,){7}\d+)/.exec(text.slice(line)) ?? [];
  if (found === undefined) {
    throw new Error(`no order at ${start + line} in ${from}`);
  }
  const at = start + line + before.length;
  await pipeline(createReadStream(from, { end: at - 1 }), createWriteStream(to));
  await pipeline([".5"], createWriteStream(to, { flags: "a" }));
  await pipeline(createReadStream(from, { start: at }), createWriteStream(to, { flags: "a" }));
  return { bytes: statSync(to).size, at };
}

const [, script, kind, rows, path] = process.argv;
if (script !== undefined && resolve(script) === fileURLToPath(import.meta.url)) {
  const write = { orders: writeOrders, hard: writeHard }[kind];
  if (write === undefined || !(Number(rows) > 0) || path === undefined) {
    console.error("usage: node bench/table-data.mjs orders|hard <rows> <file>");
    process.exit(2);
  }
  console.log(`${write(path, Number(rows))} bytes`);
}
