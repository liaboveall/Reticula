// Writes the table the big-data measurements read: a CSV of made-up orders.
//
//   node spikes/tables/data.mjs <rows> <file>
//
// The same rows every time: everything comes from one seeded generator.

import { closeSync, openSync, writeSync } from "node:fs";

const rows = Number(process.argv[2]);
const path = process.argv[3];
if (!(rows > 0) || !path) {
  console.error("usage: node spikes/tables/data.mjs <rows> <file>");
  process.exit(2);
}

let state = 0x9e3779b9;
/** 32 random bits (xorshift32). */
function bits() {
  state ^= state << 13;
  state ^= state >>> 17;
  state ^= state << 5;
  return state >>> 0;
}
const below = (limit) => bits() % limit;

const REGIONS = ["North", "South", "East", "West", "Central", "Harbour", "Airport", "Online"];
const WORDS = ["late", "gift", "return", "exchange", "fragile", "priority", "repeat", "bulk", "online", "counter"];
const products = Array.from({ length: 2000 }, (_, index) => `P-${String(index).padStart(4, "0")} ${WORDS[index % WORDS.length]}`);

const two = (value) => String(value).padStart(2, "0");
const START = Date.UTC(2023, 0, 1);

const fd = openSync(path, "w");
writeSync(fd, "id,ordered_at,day,store,region,product,customer,quantity,unit_price,discount,amount,paid,note\n");
let at = START;
let chunk = [];
for (let id = 1; id <= rows; id++) {
  // About ten million orders in two years and a half, in order of time.
  at += below(16000);
  const date = new Date(at);
  const day = `${date.getUTCFullYear()}-${two(date.getUTCMonth() + 1)}-${two(date.getUTCDate())}`;
  const time = `${two(date.getUTCHours())}:${two(date.getUTCMinutes())}:${two(date.getUTCSeconds())}`;
  const quantity = 1 + below(12);
  const cents = 99 + below(50000);
  const discount = below(5) === 0 ? below(40) : 0;
  const amount = Math.round((quantity * cents * (100 - discount)) / 100);
  const money = (value) => `${Math.trunc(value / 100)}.${two(value % 100)}`;
  const note = below(10) === 0 ? (below(3) === 0 ? `"${WORDS[below(10)]}, ${WORDS[below(10)]}"` : `${WORDS[below(10)]} ${below(1000)}`) : "";
  chunk.push(`${id},${day} ${time},${day},${below(5000)},${REGIONS[below(8)]},${products[below(2000)]},C${String(below(1200000)).padStart(7, "0")},${quantity},${money(cents)},${discount === 0 ? "" : (discount / 100).toFixed(2)},${money(amount)},${below(20) === 0 ? "false" : "true"},${note}\n`);
  if (chunk.length === 20000) {
    writeSync(fd, chunk.join(""));
    chunk = [];
  }
}
writeSync(fd, chunk.join(""));
closeSync(fd);
