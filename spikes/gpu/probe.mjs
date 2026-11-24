// What WebGPU a machine's browsers have: an adapter, drawing into a texture
// that is read back, and presenting on a canvas.
//
//   node spikes/gpu/probe.mjs
//
// Run by hand on the CI machines (the Spikes workflow), whose answers decide
// how the renderers are compared there.

import { createServer } from "node:http";
import { chromium, firefox, webkit } from "@playwright/test";

const server = createServer((_, response) => {
  response.setHeader("content-type", "text/html");
  response.end("<!doctype html><meta charset=utf-8><canvas id=c width=64 height=64></canvas>");
});
await new Promise((ready) => server.listen(0, "127.0.0.1", ready));
const url = `http://127.0.0.1:${server.address().port}/`;

const adapter = async () => {
  const found = await navigator.gpu?.requestAdapter();
  return found ? { vendor: found.info.vendor, architecture: found.info.architecture, fallback: found.info.isFallbackAdapter } : null;
};

/** Clears a texture to orange and reads it back; no canvas is involved. */
const texture = async () => {
  const device = await (await navigator.gpu.requestAdapter()).requestDevice();
  const target = device.createTexture({ size: [64, 64], format: "rgba8unorm", usage: 0x10 | 0x1 });
  const buffer = device.createBuffer({ size: 256 * 64, usage: 0x1 | 0x8 });
  const encoder = device.createCommandEncoder();
  encoder.beginRenderPass({ colorAttachments: [{ view: target.createView(), clearValue: [1, 0.5, 0, 1], loadOp: "clear", storeOp: "store" }] }).end();
  encoder.copyTextureToBuffer({ texture: target }, { buffer, bytesPerRow: 256 }, [64, 64]);
  device.queue.submit([encoder.finish()]);
  await buffer.mapAsync(1);
  return [...new Uint8Array(buffer.getMappedRange()).slice(0, 4)];
};

/** Clears a canvas to orange and copies it to a 2D canvas in the same task. */
const canvas = async () => {
  const device = await (await navigator.gpu.requestAdapter()).requestDevice();
  const element = document.getElementById("c");
  const context = element.getContext("webgpu");
  context.configure({ device, format: navigator.gpu.getPreferredCanvasFormat(), alphaMode: "opaque" });
  const encoder = device.createCommandEncoder();
  encoder.beginRenderPass({ colorAttachments: [{ view: context.getCurrentTexture().createView(), clearValue: [1, 0.5, 0, 1], loadOp: "clear", storeOp: "store" }] }).end();
  device.queue.submit([encoder.finish()]);
  const copy = document.createElement("canvas").getContext("2d");
  copy.drawImage(element, 0, 0);
  const pixel = [...copy.getImageData(5, 5, 1, 1).data];
  await device.queue.onSubmittedWorkDone();
  return pixel;
};

const software = ["--enable-unsafe-webgpu", "--use-webgpu-adapter=swiftshader"];
const headlessSoftware = [...software, "--use-angle=swiftshader", "--enable-unsafe-swiftshader", "--enable-features=Vulkan", "--use-vulkan=swiftshader", "--disable-vulkan-surface"];
const cases = [
  ["chromium shell", chromium, {}],
  ["chromium", chromium, { channel: "chromium" }],
  ["chromium, software", chromium, { channel: "chromium", args: software }],
  ["chromium, software throughout", chromium, { channel: "chromium", args: headlessSoftware }],
  ["firefox", firefox, {}],
  ["webkit", webkit, {}],
];
for (const [name, type, options] of cases) {
  const result = { name };
  let browser;
  try {
    browser = await type.launch(options);
    result.version = browser.version();
    const page = await browser.newPage();
    const limit = (promise) => Promise.race([promise, new Promise((_, reject) => setTimeout(() => reject(new Error("no answer in 20 s")), 20_000))]);
    for (const [key, probe] of Object.entries({ adapter, texture, canvas })) {
      await page.goto(url);
      try {
        result[key] = await limit(page.evaluate(probe));
      } catch (error) {
        result[key] = String(error).slice(0, 160);
      }
      if (!result.adapter) {
        break;
      }
    }
  } catch (error) {
    result.failed = String(error).slice(0, 200);
  }
  await browser?.close().catch(() => {});
  console.log(JSON.stringify(result));
}
server.close();
