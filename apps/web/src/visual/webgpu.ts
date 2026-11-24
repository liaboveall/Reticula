import { type DrawWebGpu, drawWebGpu as draw } from "@reticula/grid/internals";

/**
 * How a frame is drawn with WebGPU, if there is a renderer that can. The tests
 * know from `WEBGPU_COMPARED` in tests/visual/visual.spec.ts whether to expect
 * one.
 */
export const drawWebGpu: DrawWebGpu | null = draw;

let device: Promise<GPUDevice> | undefined;

/** The device every picture is drawn with. */
export function gpuDevice(): Promise<GPUDevice> {
  device ??= (async () => {
    const adapter = await navigator.gpu?.requestAdapter();
    if (!adapter) {
      throw new Error("this browser offers no WebGPU adapter");
    }
    const opened = await adapter.requestDevice();
    // Drawing reads back what it drew, so an invalid command would show as a
    // blank picture without a reason.
    opened.addEventListener("uncapturederror", ({ error }) => {
      console.error(`WebGPU: ${error.message}`);
    });
    // Nothing is drawn with a device that was lost, and nothing is invalid either.
    void opened.lost.then(({ message }) => {
      console.error(`WebGPU: the device was lost: ${message}`);
    });
    return opened;
  })();
  return device;
}
