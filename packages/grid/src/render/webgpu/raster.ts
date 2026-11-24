import { resetText, textContext } from "../text-measure.ts";
import { LAYER_SIZE, STRIP_HEIGHT } from "./atlas.ts";
import { TEXTURE } from "./pipeline.ts";

/** A 2D canvas the size of a strip of the atlas, and a way to copy parts of it to a texture. */
export interface Raster {
  readonly kind: "copy" | "write";
  readonly context: CanvasRenderingContext2D;
  clear(): void;
  upload(
    texture: GPUTexture,
    x: number,
    y: number,
    width: number,
    height: number,
    to: GPUOrigin3DDict,
  ): void;
}

type Open = (canvas: HTMLCanvasElement) => CanvasRenderingContext2D;

/** A canvas in no document, which would not know by itself what language its text is in. */
function open(lang: string, context: Open): CanvasRenderingContext2D {
  const canvas = document.createElement("canvas");
  canvas.width = LAYER_SIZE;
  canvas.height = STRIP_HEIGHT;
  return Object.assign(context(canvas), lang && { lang });
}

abstract class CanvasRaster {
  readonly context: CanvasRenderingContext2D;

  constructor(context: CanvasRenderingContext2D) {
    this.context = context;
  }

  clear(): void {
    this.context.clearRect(0, 0, LAYER_SIZE, STRIP_HEIGHT);
  }
}

/** Draws the way the grid's own 2D canvas does, and copies from GPU to GPU if there is one. */
class CopiedRaster extends CanvasRaster implements Raster {
  readonly kind = "copy";
  #queue: GPUQueue;

  constructor(queue: GPUQueue, lang: string) {
    super(open(lang, textContext));
    this.#queue = queue;
  }

  upload(
    texture: GPUTexture,
    x: number,
    y: number,
    width: number,
    height: number,
    to: GPUOrigin3DDict,
  ): void {
    this.#queue.copyExternalImageToTexture(
      { source: this.context.canvas, origin: [x, y] },
      { texture, origin: to, premultipliedAlpha: true },
      [width, height],
    );
  }
}

/** Draws in main memory. */
class WrittenRaster extends CanvasRaster implements Raster {
  readonly kind = "write";
  #queue: GPUQueue;

  constructor(queue: GPUQueue, lang: string) {
    super(
      open(lang, (canvas) => {
        const context = canvas.getContext("2d", { alpha: true, willReadFrequently: true });
        if (!context) {
          throw new Error("this browser cannot draw on a 2D canvas");
        }
        resetText(context);
        return context;
      }),
    );
    this.#queue = queue;
  }

  upload(
    texture: GPUTexture,
    x: number,
    y: number,
    width: number,
    height: number,
    to: GPUOrigin3DDict,
  ): void {
    const { data } = this.context.getImageData(x, y, width, height);
    // What was divided by alpha to be read comes back unchanged.
    for (let at = 0; at < data.length; at += 4) {
      const alpha = data[at + 3] as number;
      if (alpha > 0 && alpha < 255) {
        data[at] = ((data[at] as number) * alpha) / 255;
        data[at + 1] = ((data[at + 1] as number) * alpha) / 255;
        data[at + 2] = ((data[at + 2] as number) * alpha) / 255;
      }
    }
    this.#queue.writeTexture({ texture, origin: to }, data, { bytesPerRow: width * 4 }, [
      width,
      height,
    ]);
  }
}

const TRIAL = { width: 512, height: 32, rounds: 2, size: 24, turns: 3 };
/** How much faster the one that draws in main memory has to be: it draws a little differently. */
const MUCH_FASTER = 2;

/** Milliseconds of drawing and uploading some characters that have not been drawn before. */
function trial(device: GPUDevice, raster: Raster, seed: number): number {
  const texture = device.createTexture({
    size: [TRIAL.width, TRIAL.height],
    format: "rgba8unorm",
    usage: TEXTURE.copyDst | TEXTURE.attachment,
  });
  const { context } = raster;
  context.font = `${TRIAL.size}px sans-serif`;
  let code = 0x4e00 + seed;
  let started = 0;
  // The first round only gets everything going.
  for (let round = 0; round <= TRIAL.rounds; round++) {
    if (round === 1) {
      started = performance.now();
    }
    for (let x = 0; x < TRIAL.width; x += TRIAL.size) {
      context.fillText(String.fromCharCode(code++), x, TRIAL.size);
    }
    raster.upload(texture, 0, 0, TRIAL.width, TRIAL.height, {});
  }
  const spent = performance.now() - started;
  raster.clear();
  texture.destroy();
  return spent;
}

/** The raster that takes less of the main thread's time in this browser. */
export function fasterRaster(device: GPUDevice, lang: string): Raster {
  const copied = new CopiedRaster(device.queue, lang);
  const written = new WrittenRaster(device.queue, lang);
  let copying = Number.POSITIVE_INFINITY;
  let writing = Number.POSITIVE_INFINITY;
  // In turns, and the best time of each: other things go on meanwhile.
  for (let turn = 0; turn < TRIAL.turns; turn++) {
    copying = Math.min(copying, trial(device, copied, turn * 256));
    writing = Math.min(writing, trial(device, written, turn * 256 + 128));
  }
  return writing * MUCH_FASTER < copying ? written : copied;
}
