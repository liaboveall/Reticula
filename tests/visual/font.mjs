// Writes the font of the visual regression tests: fonts/reticula-test.ttf.
//
// Every glyph is a pattern of 5 by 7 dots that follows from its code point
// alone, so the font is this project's own, small, and the same wherever it is
// made. It covers what the cases draw and no more.
//
//   node tests/visual/font.mjs          write the font
//   node tests/visual/font.mjs --check  fail if the file is not what is made here

import { mkdirSync, readFileSync, writeFileSync } from "node:fs";

const FILE = new URL("fonts/reticula-test.ttf", import.meta.url);
const FAMILY = "Reticula Test";
const EM = 1000;
const ASCENT = 880;
const DESCENT = 200;
const COLS = 5;
const ROWS = 7;

/** First and last code point of each block, and how its glyphs are laid out. */
const BLOCKS = [
  [0x20, 0x7e, "half"],
  [0xa0, 0xff, "half"],
  [0x300, 0x314, "mark"],
  [0x5d0, 0x5ea, "half"],
  [0x4e00, 0x4ec7, "full"],
  [0x1f600, 0x1f603, "full"],
];

/**
 * Advance, where the pattern's lower left corner is, and the size of a dot. A
 * mark has no advance and sits over the half-width glyph before it.
 */
const SHAPES = {
  half: { advance: EM / 2, left: 50, bottom: 0, width: 80, height: 100 },
  full: { advance: EM, left: 75, bottom: -110, width: 170, height: 130 },
  mark: { advance: 0, left: -400, bottom: 720, width: 60, height: 20 },
};

/** Glyphs that reach beyond their advance, as many italic letters do. */
const OVERHANG = new Map([
  ["f".codePointAt(0), 180],
  ["j".codePointAt(0), -180],
]);

const BLANK = new Set([0x20, 0xa0]);

/** The dots of a code point: `ROWS` rows from the top, `COLS` bits each. */
function dots(codePoint) {
  let state = Math.imul(codePoint ^ 0x9e3779b9, 0x85ebca6b) >>> 0;
  const next = () => {
    state ^= state << 13;
    state ^= state >>> 17;
    state ^= state << 5;
    state >>>= 0;
    return state;
  };
  next();
  return Array.from({ length: ROWS }, () => {
    const row = (next() >>> 7) & (2 ** COLS - 1);
    return row === 0 ? 0b00100 : row;
  });
}

/** Merges dots into rectangles: runs in a row, then equal runs of rows below. */
function rectangles(rows) {
  const done = [];
  let open = [];
  for (let row = 0; row <= ROWS; row++) {
    const bits = rows[row] ?? 0;
    const runs = [];
    for (let col = 0; col < COLS; ) {
      const start = col;
      while (col < COLS && bits & (1 << (COLS - 1 - col))) {
        col++;
      }
      if (col > start) {
        runs.push({ start, end: col, top: row, bottom: row + 1 });
      } else {
        col++;
      }
    }
    const still = [];
    for (const run of runs) {
      const above = open.find((other) => other.start === run.start && other.end === run.end);
      if (above) {
        above.bottom = row + 1;
        still.push(above);
      } else {
        still.push(run);
      }
    }
    done.push(...open.filter((other) => !still.includes(other)));
    open = still;
  }
  return done;
}

function u16(value) {
  return [(value >> 8) & 0xff, value & 0xff];
}

function u32(value) {
  return [(value >>> 24) & 0xff, (value >>> 16) & 0xff, (value >>> 8) & 0xff, value & 0xff];
}

/** One glyph: its advance, its bounds and its `glyf` record. */
function glyph(codePoint, kind) {
  const shape = SHAPES[kind];
  const left = shape.left + (OVERHANG.get(codePoint) ?? 0);
  const boxes = BLANK.has(codePoint)
    ? []
    : rectangles(dots(codePoint)).map((run) => ({
        x0: left + run.start * shape.width,
        x1: left + run.end * shape.width,
        y0: shape.bottom + (ROWS - run.bottom) * shape.height,
        y1: shape.bottom + (ROWS - run.top) * shape.height,
      }));
  if (boxes.length === 0) {
    return { advance: shape.advance, points: 0, contours: 0, bounds: null, data: [] };
  }
  const bounds = {
    x0: Math.min(...boxes.map((box) => box.x0)),
    y0: Math.min(...boxes.map((box) => box.y0)),
    x1: Math.max(...boxes.map((box) => box.x1)),
    y1: Math.max(...boxes.map((box) => box.y1)),
  };
  const ends = [];
  const flags = [];
  const xs = [];
  const ys = [];
  let x = 0;
  let y = 0;
  // Bit 1 and 2: the coordinate takes one byte; bit 4 and 5: that byte is
  // positive, or without it, that the coordinate repeats.
  const delta = (value, short, same, out) => {
    if (value === 0) {
      return same;
    }
    if (Math.abs(value) < 256) {
      out.push(Math.abs(value));
      return value > 0 ? short | same : short;
    }
    out.push(...u16(value));
    return 0;
  };
  boxes.forEach((box, index) => {
    ends.push(...u16(index * 4 + 3));
    for (const [px, py] of [
      [box.x0, box.y0],
      [box.x0, box.y1],
      [box.x1, box.y1],
      [box.x1, box.y0],
    ]) {
      flags.push(0x01 | delta(px - x, 0x02, 0x10, xs) | delta(py - y, 0x04, 0x20, ys));
      x = px;
      y = py;
    }
  });
  const data = [
    ...u16(boxes.length),
    ...u16(bounds.x0),
    ...u16(bounds.y0),
    ...u16(bounds.x1),
    ...u16(bounds.y1),
    ...ends,
    ...u16(0),
    ...flags,
    ...xs,
    ...ys,
  ];
  while (data.length % 4 !== 0) {
    data.push(0);
  }
  return { advance: shape.advance, points: boxes.length * 4, contours: boxes.length, bounds, data };
}

/** `cmap`: format 4 for the first plane and format 12 for all of it. */
function cmapTable(ranges) {
  const segments = [
    ...ranges
      .filter((range) => range.last < 0xffff)
      .map(({ first, last, start }) => ({ first, last, delta: (start - first) & 0xffff })),
    { first: 0xffff, last: 0xffff, delta: 1 },
  ];
  const search = 2 * 2 ** Math.floor(Math.log2(segments.length));
  const length4 = 16 + 8 * segments.length;
  const format4 = [
    ...u16(4),
    ...u16(length4),
    ...u16(0),
    ...u16(segments.length * 2),
    ...u16(search),
    ...u16(Math.log2(search / 2)),
    ...u16(segments.length * 2 - search),
    ...segments.flatMap((segment) => u16(segment.last)),
    ...u16(0),
    ...segments.flatMap((segment) => u16(segment.first)),
    ...segments.flatMap((segment) => u16(segment.delta)),
    ...segments.flatMap(() => u16(0)),
  ];
  const format12 = [
    ...u16(12),
    ...u16(0),
    ...u32(16 + 12 * ranges.length),
    ...u32(0),
    ...u32(ranges.length),
    ...ranges.flatMap(({ first, last, start }) => [...u32(first), ...u32(last), ...u32(start)]),
  ];
  const at4 = 4 + 8 * 2;
  return [
    ...u16(0),
    ...u16(2),
    ...[...u16(3), ...u16(1), ...u32(at4)],
    ...[...u16(3), ...u16(10), ...u32(at4 + length4)],
    ...format4,
    ...format12,
  ];
}

function nameTable() {
  const names = [
    [1, FAMILY],
    [2, "Regular"],
    [3, `${FAMILY} 1`],
    [4, FAMILY],
    [5, "Version 1.000"],
    [6, FAMILY.replace(" ", "")],
  ];
  const records = [];
  const strings = [];
  for (const [id, text] of names) {
    const bytes = [...text].flatMap((char) => u16(char.codePointAt(0)));
    records.push(
      ...u16(3),
      ...u16(1),
      ...u16(0x409),
      ...u16(id),
      ...u16(bytes.length),
      ...u16(strings.length),
    );
    strings.push(...bytes);
  }
  return [...u16(0), ...u16(names.length), ...u16(6 + records.length), ...records, ...strings];
}

function checksum(bytes) {
  let sum = 0;
  for (let at = 0; at < bytes.length; at += 4) {
    sum = (sum + bytes.readUInt32BE(at)) >>> 0;
  }
  return sum;
}

function buildFont() {
  const ranges = [];
  // Glyph 0 stands for every character the font does not have.
  const glyphs = [glyph(0, "half")];
  for (const [first, last, kind] of BLOCKS) {
    ranges.push({ first, last, start: glyphs.length });
    for (let codePoint = first; codePoint <= last; codePoint++) {
      glyphs.push(glyph(codePoint, kind));
    }
  }
  const inked = glyphs.flatMap((each) => (each.bounds ? [each.bounds] : []));
  const bounds = {
    x0: Math.min(...inked.map((box) => box.x0)),
    y0: Math.min(...inked.map((box) => box.y0)),
    x1: Math.max(...inked.map((box) => box.x1)),
    y1: Math.max(...inked.map((box) => box.y1)),
  };
  const rights = glyphs.flatMap((each) => (each.bounds ? [each.advance - each.bounds.x1] : []));
  const lefts = inked.map((box) => box.x0);

  const offsets = [0];
  for (const each of glyphs) {
    offsets.push((offsets.at(-1) ?? 0) + each.data.length);
  }

  const tables = {
    "OS/2": [
      ...u16(4),
      ...u16(EM / 2),
      ...u16(400),
      ...u16(5),
      ...u16(0),
      ...[650, 600, 0, 75, 650, 600, 0, 350, 50, 260].flatMap(u16),
      ...u16(0),
      ...Array.from({ length: 10 }, () => 0),
      // Basic Latin, Latin-1, combining marks and Hebrew; CJK ideographs.
      ...u32(0x00000843),
      ...u32(0x08000000),
      ...u32(0),
      ...u32(0),
      ...[0x52, 0x54, 0x43, 0x4c],
      ...u16(0x0040),
      ...u16(0x20),
      ...u16(0xffff),
      ...u16(ASCENT),
      ...u16(-DESCENT),
      ...u16(0),
      ...u16(ASCENT),
      ...u16(DESCENT),
      ...u32(1),
      ...u32(0),
      ...u16(500),
      ...u16(700),
      ...u16(0),
      ...u16(0x20),
      ...u16(1),
    ],
    cmap: cmapTable(ranges),
    glyf: glyphs.flatMap((each) => each.data),
    head: [
      ...u16(1),
      ...u16(0),
      ...u32(0x00010000),
      // The whole file's checksum, filled in at the end.
      ...u32(0),
      ...u32(0x5f0f3cf5),
      ...u16(0x0003),
      ...u16(EM),
      // Created and modified: no date, so that the file never changes.
      ...Array.from({ length: 16 }, () => 0),
      ...u16(bounds.x0),
      ...u16(bounds.y0),
      ...u16(bounds.x1),
      ...u16(bounds.y1),
      ...u16(0),
      ...u16(6),
      ...u16(2),
      ...u16(1),
      ...u16(0),
    ],
    hhea: [
      ...u16(1),
      ...u16(0),
      ...u16(ASCENT),
      ...u16(-DESCENT),
      ...u16(0),
      ...u16(EM),
      ...u16(Math.min(...lefts)),
      ...u16(Math.min(...rights)),
      ...u16(bounds.x1),
      ...u16(1),
      ...u16(0),
      ...u16(0),
      ...Array.from({ length: 8 }, () => 0),
      ...u16(0),
      ...u16(glyphs.length),
    ],
    hmtx: glyphs.flatMap((each) => [...u16(each.advance), ...u16(each.bounds?.x0 ?? 0)]),
    loca: offsets.flatMap(u32),
    maxp: [
      ...u16(1),
      ...u16(0),
      ...u16(glyphs.length),
      ...u16(Math.max(...glyphs.map((each) => each.points))),
      ...u16(Math.max(...glyphs.map((each) => each.contours))),
      ...u16(0),
      ...u16(0),
      ...u16(2),
      ...Array.from({ length: 16 }, () => 0),
    ],
    name: nameTable(),
    post: [...u32(0x00030000), ...u32(0), ...u16(-100), ...u16(50), ...Array(20).fill(0)],
  };

  const entries = Object.entries(tables);
  const search = 16 * 2 ** Math.floor(Math.log2(entries.length));
  const header = [
    ...u32(0x00010000),
    ...u16(entries.length),
    ...u16(search),
    ...u16(Math.log2(search / 16)),
    ...u16(entries.length * 16 - search),
  ];
  const bodies = [];
  let at = header.length + entries.length * 16;
  let headAt = 0;
  for (const [tag, data] of entries) {
    const body = Buffer.alloc(Math.ceil(data.length / 4) * 4);
    body.set(data);
    if (tag === "head") {
      headAt = at;
    }
    header.push(...Buffer.from(tag), ...u32(checksum(body)), ...u32(at), ...u32(data.length));
    bodies.push(body);
    at += body.length;
  }
  const font = Buffer.concat([Buffer.from(header), ...bodies]);
  font.writeUInt32BE((0xb1b0afba - checksum(font)) >>> 0, headAt + 8);
  return font;
}

const font = buildFont();
if (process.argv.includes("--check")) {
  if (!readFileSync(FILE).equals(font)) {
    console.error("tests/visual/fonts/reticula-test.ttf is not what tests/visual/font.mjs makes");
    process.exit(1);
  }
} else {
  mkdirSync(new URL(".", FILE), { recursive: true });
  writeFileSync(FILE, font);
  console.log(`reticula-test.ttf: ${font.length} bytes`);
}
