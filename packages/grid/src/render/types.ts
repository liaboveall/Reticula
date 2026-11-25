import type { Frame, FrameCounts } from "./frame.ts";

export type RendererKind = "canvas2d" | "webgpu";

/** What a renderer on the GPU has done so far, and what it holds. */
export interface RendererCounters {
  readonly frames: number;
  /** Submissions after the first one of a frame: everything in the atlas was in use. */
  readonly extraSubmits: number;
  /** Of the last frame. */
  readonly drawCalls: number;
  readonly quads: number;
  /** Uploads of pages' quads, and how many quads they had. */
  readonly pageUploads: number;
  readonly pageQuads: number;
  /** Groups of rows whose texts were turned into quads. */
  readonly bandsBuilt: number;
  /** Times a picture of text was found in the atlas, and was not. */
  readonly atlasHits: number;
  readonly atlasMisses: number;
  readonly atlasUploads: number;
  readonly atlasUploadedBytes: number;
  readonly atlasLayers: number;
  readonly atlasEvictions: number;
  /** Frames in which bars, or what there was at another zoom, stood for texts there was no time to make. */
  readonly deferredFrames: number;
  /** Bytes of textures and buffers, without the canvas. */
  readonly bytes: number;
  /** How text gets into the atlas: copied from a canvas on the GPU, or written from main memory. */
  readonly raster: "copy" | "write";
}

/** What draws frames onto a canvas of its own. */
export interface GridRenderer {
  readonly kind: RendererKind;
  readonly canvas: HTMLCanvasElement;
  readonly counters?: RendererCounters;
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
