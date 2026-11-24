import { alphaOf, type PackedColor } from "../color.ts";
import type { PixelBox, RectList } from "../rects.ts";
import type { Sprite } from "./atlas.ts";
import { BUFFER, INSTANCE_BYTES, PART_BYTES, PART_STRIDE, SCREEN_BYTES } from "./pipeline.ts";

/** Instead of a row: shown whichever rows are. */
export const ANY_ROW = 0x7f;

const TEXTURED = 0x80;
const LAYER_SHIFT = 8;
const WORDS = INSTANCE_BYTES / 4;
/** Sides of a quad are 16 bits. */
const LONGEST = 0xffff;

/** Quads as the GPU reads them, to be uploaded together. */
export class InstanceWriter {
  length = 0;
  #words: Int32Array;
  #shorts: Uint16Array;

  constructor(capacity = 1024) {
    this.#words = new Int32Array(capacity * WORDS);
    this.#shorts = new Uint16Array(this.#words.buffer);
  }

  /** The quads from one on, as bytes. They hold until more are added. */
  bytes(first = 0): Uint8Array<ArrayBuffer> {
    return new Uint8Array(
      this.#words.buffer as ArrayBuffer,
      first * INSTANCE_BYTES,
      (this.length - first) * INSTANCE_BYTES,
    );
  }

  /** A rectangle of one colour. */
  rect(
    x: number,
    y: number,
    width: number,
    height: number,
    color: PackedColor,
    row = ANY_ROW,
  ): void {
    if (alphaOf(color) === 0) {
      return;
    }
    for (let down = 0; down < height; down += LONGEST) {
      for (let across = 0; across < width; across += LONGEST) {
        this.#push(
          x + across,
          y + down,
          Math.min(LONGEST, width - across),
          Math.min(LONGEST, height - down),
          0,
          0,
          color,
          row,
        );
      }
    }
  }

  /** Rectangles `first` to `end` of a list. */
  rects(list: RectList, first: number, end: number): void {
    const { geometry, colors } = list;
    for (let index = first; index < end; index++) {
      const at = index * 4;
      this.rect(
        geometry[at] ?? 0,
        geometry[at + 1] ?? 0,
        geometry[at + 2] ?? 0,
        geometry[at + 3] ?? 0,
        colors[index] ?? 0,
      );
    }
  }

  /** The part of a sprite that is inside `clip`, if there is one. */
  sprite(sprite: Sprite, x: number, y: number, clip: PixelBox | null, row = ANY_ROW): void {
    let { u, v, width, height } = sprite;
    let left = x;
    let top = y;
    if (clip) {
      left = Math.max(x, clip.left);
      top = Math.max(y, clip.top);
      width = Math.min(x + width, clip.right) - left;
      height = Math.min(y + height, clip.bottom) - top;
      u += left - x;
      v += top - y;
    }
    if (width > 0 && height > 0) {
      const flags = row | TEXTURED | (sprite.layer << LAYER_SHIFT);
      this.#push(left, top, width, height, u, v, 0, flags);
    }
  }

  #push(
    x: number,
    y: number,
    width: number,
    height: number,
    u: number,
    v: number,
    color: number,
    flags: number,
  ): void {
    const at = this.length * WORDS;
    if (at === this.#words.length) {
      this.#grow();
    }
    const words = this.#words;
    const shorts = this.#shorts;
    words[at] = x;
    words[at + 1] = y;
    shorts[at * 2 + 4] = width;
    shorts[at * 2 + 5] = height;
    shorts[at * 2 + 6] = u;
    shorts[at * 2 + 7] = v;
    words[at + 4] = color;
    words[at + 5] = flags;
    this.length++;
  }

  #grow(): void {
    const words = new Int32Array(this.#words.length * 2);
    words.set(this.#words);
    this.#words = words;
    this.#shorts = new Uint16Array(words.buffer);
  }
}

/** What each frame is given anew: the size of the screen, its own quads, and where its parts are. */
export class FrameBuffer {
  readonly screen: GPUBuffer;
  readonly instances = new InstanceWriter(4096);
  #device: GPUDevice;
  #layout: GPUBindGroupLayout;
  #vertices: GPUBuffer | null = null;
  #parts: GPUBuffer | null = null;
  #group: GPUBindGroup | null = null;
  #records = new Int32Array((64 * PART_STRIDE) / 4);
  #count = 0;

  constructor(device: GPUDevice, partLayout: GPUBindGroupLayout) {
    this.#device = device;
    this.#layout = partLayout;
    this.screen = device.createBuffer({
      size: SCREEN_BYTES,
      usage: BUFFER.uniform | BUFFER.copyDst,
    });
    this.clear();
  }

  get vertices(): GPUBuffer | null {
    return this.#vertices;
  }

  get group(): GPUBindGroup | null {
    return this.#group;
  }

  /** Bytes on the GPU. */
  get size(): number {
    return SCREEN_BYTES + (this.#vertices?.size ?? 0) + (this.#parts?.size ?? 0);
  }

  clear(): void {
    this.instances.length = 0;
    this.#count = 0;
    // The frame's own quads are where they say, in any row.
    this.part(0, 0, 0, ANY_ROW);
  }

  /** Where a part of the frame is, and which of its rows are shown. Returns its offset. */
  part(x: number, y: number, firstRow: number, endRow: number): number {
    const at = (this.#count * PART_STRIDE) / 4;
    if (at === this.#records.length) {
      const records = new Int32Array(at * 2);
      records.set(this.#records);
      this.#records = records;
    }
    this.#records[at] = x;
    this.#records[at + 1] = y;
    this.#records[at + 2] = firstRow;
    this.#records[at + 3] = endRow;
    return this.#count++ * PART_STRIDE;
  }

  upload(width: number, height: number): void {
    const { queue } = this.#device;
    queue.writeBuffer(this.screen, 0, new Float32Array([width, height]));
    const bytes = this.instances.bytes();
    this.#vertices = this.#sized(this.#vertices, bytes.length, BUFFER.vertex);
    queue.writeBuffer(this.#vertices, 0, bytes);
    const parts = this.#sized(this.#parts, this.#count * PART_STRIDE, BUFFER.uniform);
    if (parts !== this.#parts) {
      this.#parts = parts;
      this.#group = this.#device.createBindGroup({
        layout: this.#layout,
        entries: [{ binding: 0, resource: { buffer: parts, size: PART_BYTES } }],
      });
    }
    queue.writeBuffer(parts, 0, this.#records, 0, (this.#count * PART_STRIDE) / 4);
  }

  destroy(): void {
    this.screen.destroy();
    this.#vertices?.destroy();
    this.#parts?.destroy();
  }

  /** A buffer of at least so many bytes: the one there is, or one with room to spare. */
  #sized(buffer: GPUBuffer | null, size: number, usage: number): GPUBuffer {
    if (buffer && buffer.size >= size) {
      return buffer;
    }
    buffer?.destroy();
    return this.#device.createBuffer({
      size: Math.max(16 * PART_STRIDE, 2 ** Math.ceil(Math.log2(size))),
      usage: usage | BUFFER.copyDst,
    });
  }
}
