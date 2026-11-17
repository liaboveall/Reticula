import type { Grid, GridStats } from "@reticula/grid";

/**
 * A window onto the running app for end-to-end tests and measurements, which
 * assert on what the grid and the engine report instead of comparing pixels.
 *
 * It is installed only when the page is opened with `?hooks` in its address,
 * so that the build under test is the build that ships.
 */
export interface Hooks {
  /** What the grid is doing right now. */
  stats(): GridStats;
  /** Scrolls to a position in CSS pixels. */
  scrollTo(position: { top?: number; left?: number }): void;
  /** Selects a cell. */
  select(row: number, col: number): void;
  /** The text of a cell, or `null` while the grid does not hold it. */
  textAt(row: number, col: number): string | null;
  /** Runs the engine's determinism self-check and resolves to its digest. */
  selfCheck(): Promise<string>;
}

declare global {
  interface Window {
    __reticula?: Hooks;
  }
}

export function hooksRequested(): boolean {
  return new URLSearchParams(window.location.search).has("hooks");
}

export function installHooks(grid: Grid, selfCheck: () => Promise<string>): () => void {
  const hooks: Hooks = {
    stats: () => grid.stats(),
    scrollTo: (position) => grid.scrollTo(position),
    select: (row, col) => grid.select({ row, col }),
    textAt: (row, col) => grid.textAt(row, col),
    selfCheck,
  };
  window.__reticula = hooks;
  return () => {
    if (window.__reticula === hooks) {
      delete window.__reticula;
    }
  };
}
