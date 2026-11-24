import type { Frame, FrameCounts } from "./frame.ts";

export type RendererKind = "canvas2d" | "webgpu";

/** What draws frames onto a canvas of its own. */
export interface GridRenderer {
  readonly kind: RendererKind;
  readonly canvas: HTMLCanvasElement;
  /** Backing size in device pixels. */
  resize(width: number, height: number): void;
  draw(frame: Frame): FrameCounts;
  destroy(): void;
}

/** An image read back: straight alpha, rows from the top. */
export interface Pixels {
  readonly width: number;
  readonly height: number;
  readonly rgba: Uint8ClampedArray;
}

/** What a comparison of two renderers may tolerate at each pixel of a frame. */
export interface FrameMasks {
  /** 1 where a text run may draw (its ink box), device pixels. */
  readonly text: Uint8Array;
  /** How many translucent rectangles cover the pixel. */
  readonly translucent: Uint8Array;
}
