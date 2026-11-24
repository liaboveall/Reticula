import { Colors, serializer } from "../color.ts";
import type { Frame } from "../frame.ts";
import { SceneBuilder } from "../scene.ts";
import type { DrawWebGpu } from "../testing.ts";
import { TextMeasurer, textContext } from "../text-measure.ts";
import type { Pixels } from "../types.ts";
import { Gpu } from "./device.ts";
import { BUFFER, TEXTURE } from "./pipeline.ts";
import { Painter } from "./renderer.ts";

const MAP_READ = 0x1;
/** Frames it may take for all texts to be there. */
const MOST_FRAMES = 1000;

const opened = new WeakMap<GPUDevice, Gpu>();

/** Draws a frame into a texture and reads it back; nothing is shown. */
export const drawWebGpu: DrawWebGpu = async (frame: Frame, device: GPUDevice): Promise<Pixels> => {
  let gpu = opened.get(device);
  if (!gpu) {
    // Like the canvas it is compared with, in no document.
    gpu = new Gpu(device, "", "");
    opened.set(device, gpu);
  }
  const format = "rgba8unorm";
  const painter = new Painter(gpu, format);
  const scenes = new SceneBuilder(
    new TextMeasurer(),
    new Colors(serializer(textContext(document.createElement("canvas")))),
  );
  const scene = scenes.build(frame);
  const { width, height } = scene;
  const texture = device.createTexture({
    size: [width, height],
    format,
    usage: TEXTURE.attachment | TEXTURE.copySrc,
  });
  const view = texture.createView();
  for (let count = 0; !painter.paint(frame, scene, view); count++) {
    if (count === MOST_FRAMES) {
      throw new Error("some of the texts never come");
    }
  }
  const bytesPerRow = Math.ceil((width * 4) / 256) * 256;
  const buffer = device.createBuffer({
    size: bytesPerRow * height,
    usage: BUFFER.mapRead | BUFFER.copyDst,
  });
  const encoder = device.createCommandEncoder();
  encoder.copyTextureToBuffer({ texture }, { buffer, bytesPerRow }, [width, height]);
  device.queue.submit([encoder.finish()]);
  await buffer.mapAsync(MAP_READ);
  const rows = new Uint8Array(buffer.getMappedRange());
  const rgba = new Uint8ClampedArray(width * height * 4);
  for (let y = 0; y < height; y++) {
    rgba.set(rows.subarray(y * bytesPerRow, y * bytesPerRow + width * 4), y * width * 4);
  }
  buffer.destroy();
  texture.destroy();
  painter.destroy();
  return { width, height, rgba };
};
