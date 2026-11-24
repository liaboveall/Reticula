import type { RendererResult } from "../generated/benchmarks.ts";

/** What the kernel is written in, and what it becomes. */
export const KERNEL = ["Rust", "WebAssembly"].join(" → ");

/** What the renderers are called, after the browsers' interfaces they draw with. */
export const RENDERER_NAMES: Readonly<Record<RendererResult["renderer"], string>> = {
  canvas2d: "Canvas 2D",
  webgpu: "WebGPU",
};
