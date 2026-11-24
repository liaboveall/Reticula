import { Canvas2dRenderer } from "./canvas2d.ts";
import { Colors, serializer } from "./color.ts";
import type { Frame } from "./frame.ts";
import { masksOfScene } from "./masks.ts";
import { SceneBuilder } from "./scene.ts";
import { TextMeasurer, textContext } from "./text-measure.ts";
import type { FrameMasks, Pixels } from "./types.ts";

/** Draws a frame on a canvas of its own and reads it back. */
export function drawCanvas2d(frame: Frame): Pixels {
  const renderer = new Canvas2dRenderer();
  const width = Math.round(frame.size.width * frame.scale);
  const height = Math.round(frame.size.height * frame.scale);
  renderer.resize(width, height);
  renderer.draw(frame);
  const context = renderer.canvas.getContext("2d");
  if (!context) {
    throw new Error("the canvas has lost its context");
  }
  const rgba = context.getImageData(0, 0, width, height).data;
  renderer.destroy();
  return { width, height, rgba };
}

/** Where a frame has text, and how many translucent rectangles are on each pixel. */
export function masksOf(frame: Frame): FrameMasks {
  const context = textContext(document.createElement("canvas"));
  const builder = new SceneBuilder(new TextMeasurer(), new Colors(serializer(context)));
  return masksOfScene(builder.build(frame), builder.measure);
}

/** Draws a frame with WebGPU into a texture and reads it back. */
export type DrawWebGpu = (frame: Frame, device: GPUDevice) => Promise<Pixels>;
