import { MAX_ZOOM, MIN_ZOOM } from "@reticula/grid";
import { type MouseEvent, Suspense, useRef, useState, useSyncExternalStore } from "react";
import type { EditorHandle } from "../commands/handle.ts";
import { fmtInt, t } from "../i18n/index.ts";
import { lazyPart } from "./load-later.ts";

const LazyZoomMenu = lazyPart(({ ZoomMenu }) => ZoomMenu);

/** The grid's zoom for those who show it: a gesture changes it with every frame. */
export class ZoomSignal {
  #zoom = 1;
  #listeners = new Set<() => void>();

  get = (): number => this.#zoom;

  set = (zoom: number): void => {
    this.#zoom = zoom;
    for (const listener of this.#listeners) {
      listener();
    }
  };

  subscribe = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  };
}

export interface ZoomControlProps {
  readonly zoom: ZoomSignal;
  readonly handle: EditorHandle;
  readonly onStep: (direction: 1 | -1) => void;
}

/** A click leaves the keyboard where it is, which is most often the grid. */
export const keepFocus = (event: MouseEvent) => event.preventDefault();

export function ZoomControl({ zoom: signal, handle, onStep }: ZoomControlProps) {
  const zoom = useSyncExternalStore(signal.subscribe, signal.get, signal.get);
  const [isOpen, setIsOpen] = useState(false);
  const button = useRef<HTMLButtonElement>(null);
  const percent = `${fmtInt(Math.round(zoom * 100))}%`;
  const isLeast = zoom <= MIN_ZOOM;
  const isMost = zoom >= MAX_ZOOM;
  return (
    <fieldset className="zoom-control" aria-label={t("Zoom")}>
      <button
        type="button"
        className="zoom-btn"
        data-testid="zoom-out"
        aria-label={t("Zoom out")}
        title={t("Zoom out")}
        aria-disabled={isLeast}
        onMouseDown={keepFocus}
        onClick={() => !isLeast && onStep(-1)}
      >
        −
      </button>
      <button
        ref={button}
        type="button"
        className="zoom-btn zoom-value"
        data-testid="zoom-value"
        aria-label={t("Zoom: {percent}", { percent })}
        title={t("Zoom: {percent}", { percent })}
        aria-haspopup="menu"
        aria-expanded={isOpen}
        onMouseDown={keepFocus}
        onClick={() => setIsOpen((open) => !open)}
      >
        {percent}
      </button>
      <button
        type="button"
        className="zoom-btn"
        data-testid="zoom-in"
        aria-label={t("Zoom in")}
        title={t("Zoom in")}
        aria-disabled={isMost}
        onMouseDown={keepFocus}
        onClick={() => !isMost && onStep(1)}
      >
        +
      </button>
      {isOpen && (
        <Suspense fallback={null}>
          <LazyZoomMenu handle={handle} anchor={button} onClose={() => setIsOpen(false)} />
        </Suspense>
      )}
    </fieldset>
  );
}
