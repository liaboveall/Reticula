// Checks candidate rules for comparison, rounding and MOD against what Excel
// actually returned (the output of probe-numeric.ps1), and prints how many
// cases each rule reproduces.
//
//   node spikes/excel/check-numeric.mjs spikes/excel/results/numeric-excel-16.0.20711.tsv
//
// The rules that survive are the specification for the engine. Exits with a
// failure if a rule that is expected to hold everywhere does not.
import { readFileSync } from "node:fs";

const view = new DataView(new ArrayBuffer(8));
const fromHex = (h) => (view.setBigUint64(0, BigInt(`0x${h}`)), view.getFloat64(0));
const toHex = (x) => (view.setFloat64(0, x === 0 ? 0 : x), view.getBigUint64(0).toString(16).toUpperCase().padStart(16, "0"));
const isNumber = (field) => /^[0-9A-F]{16}$/.test(field);

// ---------------------------------------------------------------- the rules

/** x rounded to 15 significant decimal digits, halfway cases away from zero. */
const round15 = (x) => (x === 0 || !Number.isFinite(x) ? x : Number(x.toPrecision(15)));

/** The 15 significant digits of |x| as an integer D and a scale: |x| ~ D / 10^scale. */
function digits15(x) {
  const [mantissa, exponent] = Math.abs(x).toExponential(14).split("e");
  return { negative: x < 0, digits: BigInt(mantissa.replace(".", "")), scale: 14 - Number(exponent) };
}

/** Decimal rounding of the 15-digit value of x to `places` decimal places. */
function roundDecimal(x, places, mode) {
  if (x === 0) return 0;
  const { negative, digits, scale } = digits15(x);
  let quotient = digits;
  let kept = scale;
  if (places < scale) {
    const divisor = 10n ** BigInt(scale - places);
    const remainder = digits % divisor;
    quotient = digits / divisor;
    kept = places;
    if (mode === "half" ? remainder * 2n >= divisor : mode === "away" ? remainder > 0n : false) quotient += 1n;
  }
  const magnitude = Number(`${quotient}e${-kept}`); // the correctly rounded double of the decimal result
  return negative && magnitude !== 0 ? -magnitude : magnitude;
}

const truncate = (x) => roundDecimal(x, 0, "toward-zero");
const floor15 = (x) => (x < 0 ? roundDecimal(x, 0, "away") : truncate(x));
const ceiling15 = (x) => (x > 0 ? roundDecimal(x, 0, "away") : truncate(x));
/** INT: below 10^15 the floor of the 15-digit value; from there on, the exact floor. */
const int = (x) => (Math.abs(x) < 1e15 ? floor15(x) : Math.floor(x));

/** MOD: the exact remainder, with the sign of the divisor. */
function mod(a, b) {
  let remainder = a % b;
  if (remainder !== 0 && remainder < 0 !== b < 0) remainder += b;
  return remainder;
}

// ------------------------------------------------------------------ the data

const cases = { cmp: [], rnd: [], near: [], mod: [], cond: [], lit: [] };
let altered = 0;
for (const line of readFileSync(process.argv[2], "utf8").split("\n")) {
  if (!line || line.startsWith("#")) continue;
  const [id, kind, a, b, stored, ...results] = line.split("\t");
  if (kind === "lit") continue;
  if (stored !== "=") {
    altered++; // Excel did not keep the input: subnormal numbers and negative zero become 0
    continue;
  }
  cases[kind].push({ id, a: fromHex(a), b: fromHex(b), aBits: a, bBits: b, results });
}

const report = [];
let failed = false;
function check(name, kind, expectAll, matches) {
  const total = cases[kind].length;
  const hits = cases[kind].filter(matches).length;
  if (expectAll && hits !== total) failed = true;
  report.push(`${hits === total ? "all " : "    "}${String(hits).padStart(6)} / ${String(total).padEnd(6)} ${name}`);
}
const truth = (field) => field === "TRUE";
const one = "3FF0000000000000";

// --- comparison
const operators = [["=", (a, b) => a === b], ["<>", (a, b) => a !== b], ["<", (a, b) => a < b], [">", (a, b) => a > b], ["<=", (a, b) => a <= b], [">=", (a, b) => a >= b]];
operators.forEach(([symbol, compare], column) => {
  check(`a ${symbol} b: compare after rounding both sides to 15 digits`, "cmp", true, (c) => truth(c.results[column]) === compare(round15(c.a), round15(c.b)));
});
check("a = b: plain IEEE comparison (for contrast)", "cmp", false, (c) => truth(c.results[0]) === (c.a === c.b));
check("a = b: relative difference within 1e-15 (for contrast)", "cmp", false, (c) => truth(c.results[0]) === Math.abs(c.a - c.b) <= 1e-15 * Math.max(Math.abs(c.a), Math.abs(c.b)));
check("a = b: rounding to 16 digits instead (for contrast)", "cmp", false, (c) => truth(c.results[0]) === (Number(c.a.toPrecision(16)) === Number(c.b.toPrecision(16)) || c.a === c.b));
check("(a-b), not the last operation: exact IEEE subtraction", "cmp", true, (c) => c.results[7] === toHex(c.a - c.b));
check("a-b as the last operation: exact IEEE subtraction (it is not: small results become 0)", "cmp", false, (c) => c.results[6] === toHex(c.a - c.b));
check("DELTA(a,b): equal only when bit-identical", "cmp", true, (c) => (c.results[9] === one) === (c.aBits === c.bBits));

// --- conditions and lookups
const equal15 = (c) => round15(c.a) === round15(c.b);
const identical = (c) => c.aBits === c.bBits;
check("COUNTIF(b, a): same rule as =", "cond", true, (c) => (c.results[1] === one) === equal15(c));
check("SUMIF(b, a, b): same rule as =", "cond", true, (c) => truth(c.results[4]) === equal15(c));
check('COUNTIF(b, "="&a): same rule as =', "cond", true, (c) => (c.results[5] === one) === equal15(c));
check("MATCH(a, b, 0): finds b only when bit-identical", "cond", true, (c) => (c.results[2] === one) === identical(c));
check("VLOOKUP(a, b, 1, FALSE): finds b only when bit-identical", "cond", true, (c) => truth(c.results[3]) === identical(c));

// --- rounding to a number of decimal places
check("ROUND(x, d): 15 digits, then halves away from zero", "rnd", true, (c) => c.results[0] === toHex(roundDecimal(c.a, c.b, "half")));
check("ROUND(x, d): rounding the binary value directly (for contrast)", "rnd", false, (c) => c.results[0] === toHex((Math.sign(c.a) * Math.round(Math.abs(c.a) * 10 ** c.b)) / 10 ** c.b));
check("ROUNDDOWN(x, d): 15 digits, then toward zero", "rnd", true, (c) => c.results[2] === toHex(roundDecimal(c.a, c.b, "toward-zero")));
check("TRUNC(x, d): 15 digits, then toward zero", "rnd", true, (c) => c.results[3] === toHex(roundDecimal(c.a, c.b, "toward-zero")));
check("ROUNDUP(x, d): 15 digits, then away from zero, equal to Excel at 15 digits", "rnd", true, (c) => round15(fromHex(c.results[1])) === round15(roundDecimal(c.a, c.b, "away")));
check("ROUNDUP(x, d): the same, bit for bit (Excel adds one unit to ROUNDDOWN in floating point)", "rnd", false, (c) => c.results[1] === toHex(roundDecimal(c.a, c.b, "away")));
check("ROUNDUP(x, d): ROUNDDOWN plus one unit, added in floating point", "rnd", true, (c) => {
  const down = roundDecimal(c.a, c.b, "toward-zero");
  const up = down === roundDecimal(c.a, c.b, "away") ? down : down + Math.sign(c.a) * 10 ** -c.b;
  return c.results[1] === toHex(up);
});

// --- rounding to an integer
check("TRUNC(x): 15 digits, then toward zero", "near", true, (c) => c.results[1] === toHex(truncate(c.a)));
check("FLOOR(x, 1): 15 digits, then down", "near", true, (c) => c.results[2] === toHex(floor15(c.a)));
check("CEILING(x, 1): 15 digits, then up", "near", true, (c) => c.results[3] === toHex(ceiling15(c.a)));
check("INT(x): 15 digits then down below 1e15, exact floor from 1e15", "near", true, (c) => c.results[0] === toHex(int(c.a)));
check("INT(x): plain floor (for contrast)", "near", false, (c) => c.results[0] === toHex(Math.floor(c.a)));
check("ROUND(x, 0): 15 digits, then halves away (Excel differs on exact 16-digit ties)", "near", false, (c) => c.results[4] === toHex(roundDecimal(c.a, 0, "half")));

// --- MOD
const answered = (c) => isNumber(c.results[0]);
const modTotal = cases.mod.length;
cases.mod = cases.mod.filter(answered);
report.push(`    ${String(modTotal - cases.mod.length).padStart(6)} / ${String(modTotal).padEnd(6)} MOD(a, b) is #NUM! (every quotient of 1e13 or more; none of 1e12 or less)`);
check("MOD(a, b): exact remainder with the sign of b", "mod", true, (c) => c.results[0] === toHex(mod(c.a, c.b)));
check("MOD(a, b): a - b*floor(a/b) in floating point (for contrast)", "mod", false, (c) => c.results[0] === toHex(c.a - c.b * Math.floor(c.a / c.b)));
check("MOD(a, b): on operands rounded to 15 digits (for contrast)", "mod", false, (c) => c.results[0] === toHex(mod(round15(c.a), round15(c.b))));

console.log(report.join("\n"));
console.log(`\n${altered} cases skipped: Excel stored 0 in place of a subnormal number or of negative zero.`);
if (failed) {
  console.log("\nA rule that was expected to hold for every case does not.");
  process.exit(1);
}
