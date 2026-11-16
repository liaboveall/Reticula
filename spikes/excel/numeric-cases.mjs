// Generates the inputs for probe-numeric.ps1: numbers chosen to sit on the
// boundaries where a spreadsheet's comparison and rounding rules can differ
// from plain IEEE 754 arithmetic.
//
//   node spikes/excel/numeric-cases.mjs > spikes/excel/data/numeric-cases.tsv
//
// Numbers are written as the 16 hex digits of their bit pattern, so that no
// decimal conversion stands between this file and the cell.

const view = new DataView(new ArrayBuffer(8));
const bits = (x) => (view.setFloat64(0, x), view.getBigUint64(0));
const fromBits = (b) => (view.setBigUint64(0, BigInt.asUintN(64, b)), view.getFloat64(0));
const hex = (x) => bits(x).toString(16).toUpperCase().padStart(16, "0");

/** Moves x by k units in the last place; k > 0 is toward positive infinity. */
function ulps(x, k) {
  if (k === 0) return x;
  if (x === 0) return k > 0 ? fromBits(BigInt(k)) : -fromBits(BigInt(-k));
  const b = bits(x);
  const negative = b >> 63n === 1n;
  const magnitude = (b & 0x7fff_ffff_ffff_ffffn) + BigInt(negative ? -k : k);
  return fromBits((negative ? 1n << 63n : 0n) | magnitude);
}

// SplitMix64, so that the file is the same on every run.
let state = 0x5eedn;
function random() {
  state = BigInt.asUintN(64, state + 0x9e3779b97f4a7c15n);
  let z = state;
  z = BigInt.asUintN(64, (z ^ (z >> 30n)) * 0xbf58476d1ce4e5b9n);
  z = BigInt.asUintN(64, (z ^ (z >> 27n)) * 0x94d049bb133111ebn);
  return Number((z ^ (z >> 31n)) >> 11n) / 2 ** 53;
}

const both = (ks) => ks.flatMap((k) => (k === 0 ? [0] : [k, -k]));
const rows = [];
const add = (kind, a, b, note) => rows.push([kind, hex(a), hex(b), note]);

// --- cmp: a number against a neighbour k units in the last place away.
const cmpSteps = both([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 16, 20, 24, 32, 48, 64, 96, 128, 256, 512, 1024, 4096, 65536]);
const named = [
  1, 2, 3, 10, 0.1, 0.2, 0.3, 0.7, 1 / 3, 2 / 3, 0.5, 1.5, 100, 1234.5678, 123456.789, 1e6, 1e9,
  999999999999999, 1e15, 2 ** 52, 2 ** 53, 1e16, 1e100, 1e300, 1e-5, 1e-10, 1e-100, 1e-300,
  0.1 + 0.2, 4.35, 435, 8.345, Math.PI, Math.E, 0.999999999999999, 1.00000000000001,
  9.99999999999999, 99999.9999999999,
];
const scattered = Array.from({ length: 150 }, () => (1 + 9 * random()) * 10 ** Math.floor(random() * 61 - 30));
const cmpBases = [...named, ...scattered, ...named.slice(0, 12).map((x) => -x), ...scattered.slice(0, 30).map((x) => -x)];
for (const base of cmpBases) for (const k of cmpSteps) add("cmp", base, ulps(base, k), `${k} ulp`);
for (const tiny of [5e-324, 2.2250738585072014e-308, 1e-300, 1e-100, 1e-20, 1e-16, 1e-15, -0, -5e-324, -1e-300]) add("cmp", 0, tiny, "zero");

// --- rnd: ROUND and friends just below, at, and just above a decimal half.
const roundSteps = both([0, 1, 2, 3, 4, 5, 8, 16, 32, 64, 256, 1024]);
const halves = [5, 15, 25, 35, 45, 55, 65, 75, 85, 95, 105, 115, 125, 285, 1005, 1115, 2675, 8345, 99995, 123455, 1234565, 49999995];
for (let digits = 0; digits <= 8; digits++) {
  for (const n of halves) {
    for (const sign of ["", "-"]) {
      const exact = Number(`${sign}${n}e-${digits + 1}`);
      for (const k of roundSteps) add("rnd", ulps(exact, k), digits, `${sign}${n}e-${digits + 1} ${k} ulp`);
    }
  }
}
for (const [value, digits] of [[5, -1], [15, -1], [25, -1], [125, -1], [1005, -1], [50, -2], [150, -2], [250, -2], [1050, -2], [-15, -1], [-250, -2]]) {
  for (const k of roundSteps) add("rnd", ulps(value, k), digits, `${value} ${k} ulp`);
}

// --- near: INT, TRUNC, FLOOR and CEILING around integers.
const nearSteps = both([0, 1, 2, 3, 4, 5, 6, 8, 12, 16, 32, 64, 128, 1024, 65536]);
const integers = [1, 2, 7, 8, 9, 10, 80, 99, 100, 1000, 65536, 1e6, 123456789, 1e9, 1e12, 1e14, 999999999999999, 1e15, 2 ** 52, 2 ** 53];
for (const n of integers) for (const sign of [1, -1]) for (const k of nearSteps) add("near", ulps(sign * n, k), 0, `${sign * n} ${k} ulp`);
for (const k of nearSteps.filter((k) => k !== 0)) add("near", ulps(0, k), 0, `0 ${k} ulp`);
add("near", 0, 0, "0");

// --- mod: decimal divisors, near-multiples, large quotients, negative operands.
const dividends = ["1", "2", "3", "5", "7.5", "10", "0.3", "0.7", "1.2", "12.34", "100", "1000", "1e6", "123456.789", "1e10", "1e15", "-1", "-7.5", "-10", "0.1", "0.5", "5.5", "2.5", "1e20", "9007199254740992"];
const divisors = ["0.1", "0.01", "0.2", "0.3", "0.5", "0.7", "1", "1.5", "3", "7", "0.001", "1e-5", "-0.1", "-3", "-0.3", "10", "1e5", "0.25"];
for (const a of dividends) for (const b of divisors) add("mod", Number(a), Number(b), `${a} mod ${b}`);
for (const b of [0.1, 0.3, 3, 0.01, 0.7]) {
  for (const q of [3, 10, 33, 1000, 123457]) {
    for (let k = -4; k <= 4; k++) add("mod", ulps(b * q, k), b, `${b}*${q} ${k} ulp`);
  }
}
for (const a of [1e15, 1e16, 1e17, 1e20, 2 ** 53, 2 ** 60, 134217728 * 3 + 1, 2 ** 50, 2 ** 50 * 3 + 1]) {
  for (const b of [3, 7, 0.1, 1]) add("mod", a, b, "large quotient");
}

// --- cond: do COUNTIF, MATCH and lookups use the same notion of equality as "="?
const condSteps = both([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 16, 32, 64, 128]);
for (const base of [1, 0.3, 0.1 + 0.2, 123456.789, 1e15, 1e-5, -0.3, 1 / 3, 100, 0.1]) {
  for (const k of condSteps) add("cond", base, ulps(base, k), `${k} ulp`);
}

// --- lit: formulas typed as text, where the arithmetic itself is the subject.
const formulas = [
  "=0.1+0.2", "=0.1+0.2=0.3", "=0.1+0.2-0.3", "=(0.1+0.2-0.3)", "=(0.1+0.2)-0.3", "=0.1+(0.2-0.3)",
  "=1-0.9", "=1-0.9-0.1", "=(1-0.9-0.1)", "=0.3-0.2-0.1", "=0.5-0.4-0.1", "=1.1+2.2-3.3", "=(1.1+2.2-3.3)",
  "=0.1*3", "=0.1*3=0.3", "=0.1*3-0.3", "=(0.1*3-0.3)", "=1.1*1.1", "=1.1*1.1=1.21", "=1.1*1.1-1.21",
  "=4.35*100", "=INT(4.35*100)", "=4.35*100=435", "=4.35*100-435", "=(0.1+0.7)*10", "=INT((0.1+0.7)*10)",
  "=1.1*3", "=1.1*3=3.3", "=3*1.1-3.3", "=SUM(0.1,0.2)-0.3", "=SUM(0.1,0.2,-0.3)", "=SUM(0.1,0.2)=0.3",
  "=0.1+0.2-0.3+0", "=0+(0.1+0.2-0.3)", "=-(0.3-0.1-0.2)", "=(0.1+0.2-0.3)*1", "=0.1+0.2-0.3=0", "=(0.1+0.2-0.3)=0",
  "=1E+16+1", "=1E+16+1-1E+16", "=SUM(1E+16,1,-1E+16)", "=1E+15+0.3-1E+15", "=(1E+15+0.3)-1E+15",
  "=100000000000000.3-100000000000000", "=1000.3-1000", "=1000.3-1000-0.3", "=1000.3-1000=0.3",
  "=ROUND(2.5,0)", "=ROUND(-2.5,0)", "=ROUND(1.005,2)", "=ROUND(2.675,2)", "=ROUND(1.115,2)", "=ROUND(0.285,2)",
  "=MOD(1,0.1)", "=MOD(10,0.1)", "=MOD(0.3,0.1)", "=MOD(1.2,0.1)", "=MOD(-1,3)", "=MOD(1,-3)", "=MOD(5.5,1.5)",
  "=1/3*3=1", "=1/3+1/3+1/3=1", "=2^0.5*2^0.5=2", "=SQRT(2)^2-2", "=(SQRT(2)^2-2)", "=10^15+1", "=10^15+1=10^15",
  "=0.1+0.2>0.3", "=0.1+0.2<0.3", "=IF(0.1+0.2=0.3,1,0)", "=1=1.000000000000001", "=1=1.0000000000000001",
];
for (const formula of formulas) rows.push(["lit", "", "", formula]);

console.log("id\tkind\ta\tb\tnote");
rows.forEach((row, index) => console.log([index + 1, ...row].join("\t")));
