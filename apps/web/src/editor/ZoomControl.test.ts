import { ZOOM_STEPS } from "@reticula/grid";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { ZoomControl, ZoomSignal } from "./ZoomControl.tsx";

function markup(zoom: number): string {
  const signal = new ZoomSignal();
  signal.set(zoom);
  return renderToStaticMarkup(
    createElement(ZoomControl, { zoom: signal, onStep: () => {}, onZoom: () => {} }),
  );
}

describe("the zoom control", () => {
  it("has a name for each of its buttons, and says the zoom in its own", () => {
    const html = markup(0.67);
    const names = [...html.matchAll(/<button[^>]*aria-label="([^"]+)"/g)].map(([, name]) => name);
    expect(names).toEqual(["Zoom out", "Zoom: 67%", "Zoom in"]);
    expect(html).toContain(">67%<");
    expect(html).toContain('aria-haspopup="menu"');
    expect(html).toContain('aria-expanded="false"');
  });

  it("has nothing to offer past the limits", () => {
    const disabled = (zoom: number) =>
      [...markup(zoom).matchAll(/data-testid="zoom-(in|out)"[^>]*aria-disabled="(\w+)"/g)].map(
        ([, , isDisabled]) => isDisabled,
      );
    expect(disabled(ZOOM_STEPS[0] ?? 0)).toEqual(["true", "false"]);
    expect(disabled(1)).toEqual(["false", "false"]);
    expect(disabled(ZOOM_STEPS.at(-1) ?? 0)).toEqual(["false", "true"]);
  });
});

describe("the grid's zoom", () => {
  it("is told to those who show it, until they no longer do", () => {
    const signal = new ZoomSignal();
    const listener = vi.fn();
    const stop = signal.subscribe(listener);
    signal.set(0.5);
    expect(signal.get()).toBe(0.5);
    stop();
    signal.set(2);
    expect(listener).toHaveBeenCalledOnce();
  });
});
