import type { Grid, RendererPreference } from "@reticula/grid";

const KEY = "reticula:renderer";

/** What grids are to be drawn with: as the address says, or else the user. */
export function rendererPreference(): RendererPreference {
  const asked = new URLSearchParams(window.location.search).get("renderer");
  if (asked === "canvas2d" || asked === "webgpu") {
    return asked;
  }
  try {
    return localStorage.getItem(KEY) === "canvas2d" ? "canvas2d" : "auto";
  } catch {
    return "auto";
  }
}

/** Keeps the user's choice for later visits, and follows it now. */
export function setRendererPreference(grid: Grid | null, preference: "auto" | "canvas2d"): void {
  try {
    localStorage.setItem(KEY, preference);
  } catch {
    // It holds for this visit at least.
  }
  grid?.setRenderer(preference);
}
