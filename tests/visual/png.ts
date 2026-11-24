// PNG files of opaque pictures, written and read without a browser so that a
// golden image holds exactly the pixels that were drawn.

import { crc32, deflateSync, inflateSync } from "node:zlib";

export interface Picture {
  readonly width: number;
  readonly height: number;
  /** Four bytes a pixel. */
  readonly rgba: Uint8Array;
}

const SIGNATURE = Buffer.from("89504e470d0a1a0a", "hex");
const CHANNELS = 3;
const UNFILTERED = 0;
const LIKE_THE_LEFT = 1;
const LIKE_ABOVE = 2;

function chunk(type: string, data: Uint8Array): Buffer {
  const body = Buffer.concat([Buffer.from(type, "latin1"), data]);
  const out = Buffer.alloc(body.length + 8);
  out.writeUInt32BE(data.length, 0);
  out.set(body, 4);
  out.writeUInt32BE(crc32(body), body.length + 4);
  return out;
}

export function encodePng({ width, height, rgba }: Picture): Buffer {
  const stride = width * CHANNELS;
  const rows = Buffer.alloc(height * stride);
  for (let pixel = 0; pixel < width * height; pixel++) {
    if (rgba[pixel * 4 + 3] !== 255) {
      throw new Error(`pixel ${pixel} is not opaque`);
    }
    rows.set(rgba.subarray(pixel * 4, pixel * 4 + CHANNELS), pixel * CHANNELS);
  }
  const filtered = Buffer.alloc(height * (stride + 1));
  for (let row = 0; row < height; row++) {
    let best = Number.POSITIVE_INFINITY;
    for (const filter of [UNFILTERED, LIKE_THE_LEFT, LIKE_ABOVE]) {
      const line = Buffer.alloc(stride);
      let cost = 0;
      for (let at = 0; at < stride; at++) {
        const here = row * stride + at;
        const value = ((rows[here] ?? 0) - predicted(filter, rows, here, at, stride)) & 0xff;
        line[at] = value;
        cost += Math.min(value, 256 - value);
      }
      if (cost < best) {
        best = cost;
        filtered[row * (stride + 1)] = filter;
        filtered.set(line, row * (stride + 1) + 1);
      }
    }
  }
  const header = Buffer.alloc(13);
  header.writeUInt32BE(width, 0);
  header.writeUInt32BE(height, 4);
  // Eight bits a channel, RGB.
  header.set([8, 2, 0, 0, 0], 8);
  return Buffer.concat([
    SIGNATURE,
    chunk("IHDR", header),
    chunk("IDAT", deflateSync(filtered, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

/** Reads what `encodePng` writes; other kinds of PNG are refused. */
export function decodePng(file: Buffer): Picture {
  if (!file.subarray(0, 8).equals(SIGNATURE)) {
    throw new Error("not a PNG file");
  }
  let header: Buffer | undefined;
  const data: Buffer[] = [];
  for (let at = 8; at < file.length; ) {
    const length = file.readUInt32BE(at);
    const type = file.toString("latin1", at + 4, at + 8);
    const body = file.subarray(at + 8, at + 8 + length);
    if (type === "IHDR") {
      header = body;
    } else if (type === "IDAT") {
      data.push(body);
    }
    at += length + 12;
  }
  if (header?.subarray(8).toString("hex") !== "0802000000") {
    throw new Error("not an RGB image of eight bits a channel");
  }
  const width = header.readUInt32BE(0);
  const height = header.readUInt32BE(4);
  const stride = width * CHANNELS;
  const filtered = inflateSync(Buffer.concat(data));
  const rows = Buffer.alloc(height * stride);
  for (let row = 0; row < height; row++) {
    const filter = filtered[row * (stride + 1)] ?? 0;
    if (filter > LIKE_ABOVE) {
      throw new Error(`row ${row} is filtered in a way this does not read`);
    }
    for (let at = 0; at < stride; at++) {
      const here = row * stride + at;
      const value = filtered[row * (stride + 1) + 1 + at] ?? 0;
      rows[here] = (value + predicted(filter, rows, here, at, stride)) & 0xff;
    }
  }
  const rgba = new Uint8Array(width * height * 4).fill(255);
  for (let pixel = 0; pixel < width * height; pixel++) {
    rgba.set(rows.subarray(pixel * CHANNELS, (pixel + 1) * CHANNELS), pixel * 4);
  }
  return { width, height, rgba };
}

function predicted(
  filter: number,
  rows: Uint8Array,
  here: number,
  at: number,
  stride: number,
): number {
  if (filter === LIKE_THE_LEFT) {
    return at >= CHANNELS ? (rows[here - CHANNELS] ?? 0) : 0;
  }
  return filter === LIKE_ABOVE && here >= stride ? (rows[here - stride] ?? 0) : 0;
}
