import type { PackedColor } from "./color.ts";

/** A rectangle of whole device pixels; `right` and `bottom` are not in it. */
export interface PixelBox {
  readonly left: number;
  readonly top: number;
  readonly right: number;
  readonly bottom: number;
}

export function isEmptyBox(box: PixelBox): boolean {
  return box.left >= box.right || box.top >= box.bottom;
}

/** Axis-aligned rectangles of whole device pixels, in the order they are painted. */
export class RectList {
  length = 0;
  /** Left, top, width and height of each rectangle. */
  geometry: Int32Array;
  colors: Uint32Array;

  constructor(capacity = 64) {
    this.geometry = new Int32Array(capacity * 4);
    this.colors = new Uint32Array(capacity);
  }

  clear(): void {
    this.length = 0;
  }

  push(left: number, top: number, width: number, height: number, color: PackedColor): void {
    if (width <= 0 || height <= 0) {
      return;
    }
    if (this.length === this.colors.length) {
      const geometry = new Int32Array(this.geometry.length * 2);
      geometry.set(this.geometry);
      this.geometry = geometry;
      const colors = new Uint32Array(this.colors.length * 2);
      colors.set(this.colors);
      this.colors = colors;
    }
    const at = this.length * 4;
    this.geometry[at] = left;
    this.geometry[at + 1] = top;
    this.geometry[at + 2] = width;
    this.geometry[at + 3] = height;
    this.colors[this.length] = color;
    this.length++;
  }

  /**
   * The part of a rectangle inside `clip`. Edges may be far outside: whole
   * columns of a million rows are, and are cut down before they are stored.
   */
  add(
    clip: PixelBox,
    left: number,
    top: number,
    right: number,
    bottom: number,
    color: PackedColor,
  ): void {
    const x = Math.max(clip.left, left);
    const y = Math.max(clip.top, top);
    this.push(x, y, Math.min(clip.right, right) - x, Math.min(clip.bottom, bottom) - y, color);
  }
}

/** Width of a line in device pixels. */
export function lineWidth(width: number, scale: number): number {
  return Math.max(1, Math.round(width * scale));
}

/** A frame just inside the given edges: four rectangles that do not overlap. */
export function addOutline(
  rects: RectList,
  clip: PixelBox,
  left: number,
  top: number,
  right: number,
  bottom: number,
  width: number,
  color: PackedColor,
): void {
  if (right - left <= 2 * width || bottom - top <= 2 * width) {
    rects.add(clip, left, top, right, bottom, color);
    return;
  }
  rects.add(clip, left, top, right, top + width, color);
  rects.add(clip, left, bottom - width, right, bottom, color);
  rects.add(clip, left, top + width, left + width, bottom - width, color);
  rects.add(clip, right - width, top + width, right, bottom - width, color);
}

/**
 * A dashed frame just inside the given edges. The dashes run clockwise from the
 * top left corner, wherever that is: the ones in view stay where they are
 * while the rest scrolls, and only they are stored.
 */
export function addDashedOutline(
  rects: RectList,
  clip: PixelBox,
  left: number,
  top: number,
  right: number,
  bottom: number,
  width: number,
  on: number,
  off: number,
  color: PackedColor,
): void {
  // Each edge has the corner it starts from and leaves the other one to the next edge.
  const across = Math.max(0, right - left - width);
  const down = Math.max(0, bottom - top - width);
  const period = on + off;

  /** Calls `dash` with the stretches of an edge to paint, in distances from its start. */
  const edge = (
    start: number,
    length: number,
    from: number,
    to: number,
    dash: (from: number, to: number) => void,
  ): void => {
    const first = Math.max(0, from);
    const end = Math.min(length, to);
    if (first >= end) {
      return;
    }
    const last = Math.floor((start + end - 1) / period);
    for (let index = Math.floor((start + first) / period); index <= last; index++) {
      const at = index * period - start;
      const a = Math.max(first, at);
      const b = Math.min(end, at + on);
      if (a < b) {
        dash(a, b);
      }
    }
  };

  if (top < clip.bottom && top + width > clip.top) {
    edge(0, across, clip.left - left, clip.right - left, (a, b) =>
      rects.add(clip, left + a, top, left + b, top + width, color),
    );
  }
  if (right - width < clip.right && right > clip.left) {
    edge(across, down, clip.top - top, clip.bottom - top, (a, b) =>
      rects.add(clip, right - width, top + a, right, top + b, color),
    );
  }
  if (bottom - width < clip.bottom && bottom > clip.top) {
    edge(across + down, across, right - clip.right, right - clip.left, (a, b) =>
      rects.add(clip, right - b, bottom - width, right - a, bottom, color),
    );
  }
  if (left < clip.right && left + width > clip.left) {
    edge(2 * across + down, down, bottom - clip.bottom, bottom - clip.top, (a, b) =>
      rects.add(clip, left, bottom - b, left + width, bottom - a, color),
    );
  }
}
