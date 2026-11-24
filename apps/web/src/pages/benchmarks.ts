import type { GpuResult, GpuRun } from "../generated/benchmarks.ts";
import { fmtInt, t } from "../i18n/index.ts";

export function rendererName(renderer: GpuResult["renderer"]): string {
  return renderer === "webgpu" ? "WebGPU" : t("Canvas 2D");
}

const MOTIONS: Readonly<Record<GpuResult["motion"], () => string>> = {
  scroll: () => t("scrolling"),
  jump: () => t("jumping"),
  zoom: () => t("zooming"),
  edit: () => t("editing"),
};

/** What was drawn, in words. */
export function workloadName({ part, size, motion, content, cells }: GpuResult): string {
  const name = t("{cells} cells, {motion}", {
    cells: part === "app" ? fmtInt(cells ?? 0) : size === "1k" ? "1,000" : "10,000+",
    motion: MOTIONS[motion](),
  });
  return part === "app"
    ? t("App: {name}", { name })
    : content === "repeating"
      ? t("{name}, repeating texts", { name })
      : name;
}

/** The latest run that shows what the renderers do where nothing is held back. */
export function latestComparison(runs: readonly GpuRun[]): GpuRun | null {
  return (
    runs.findLast(
      ({ slowdown, dpr, browser }) => slowdown === 1 && dpr === 1 && browser?.startsWith("Chrome "),
    ) ?? null
  );
}

export interface Comparison {
  readonly name: string;
  readonly canvas2d: GpuResult;
  readonly webgpu: GpuResult;
}

/** The renderers side by side: the workloads whose texts are all different, which are the harder ones. */
export function comparisons({ results }: GpuRun): readonly Comparison[] {
  return results.flatMap((canvas2d) => {
    const { part, size, motion, content, renderer } = canvas2d;
    const webgpu = results.find(
      (one) =>
        one.renderer === "webgpu" &&
        one.part === part &&
        one.size === size &&
        one.motion === motion &&
        one.content === content,
    );
    return renderer === "canvas2d" && content === "distinct" && webgpu
      ? [{ name: workloadName(canvas2d), canvas2d, webgpu }]
      : [];
  });
}

/** Where milliseconds are on a scale of four decades from 0.1 on, as a share of it. */
export function logPosition(ms: number | null): number {
  return Math.min(1, Math.max(0, (Math.log10(Math.max(ms ?? 0, 0.1)) + 1) / 4));
}
