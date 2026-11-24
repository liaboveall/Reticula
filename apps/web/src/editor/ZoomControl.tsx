import { MAX_ZOOM, MIN_ZOOM, ZOOM_STEPS } from "@reticula/grid";
import {
  type KeyboardEvent,
  type MouseEvent,
  useEffect,
  useId,
  useRef,
  useState,
  useSyncExternalStore,
} from "react";
import { fmtInt, t } from "../i18n/index.ts";

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
  readonly onStep: (direction: 1 | -1) => void;
  readonly onZoom: (zoom: number) => void;
}

const percent = (zoom: number) => `${fmtInt(Math.round(zoom * 100))}%`;

/** A click leaves the keyboard where it is, which is most often the grid. */
const keepFocus = (event: MouseEvent) => event.preventDefault();

export function ZoomControl({ zoom: signal, onStep, onZoom }: ZoomControlProps) {
  const zoom = useSyncExternalStore(signal.subscribe, signal.get, signal.get);
  const [isOpen, setIsOpen] = useState(false);
  const button = useRef<HTMLButtonElement>(null);
  const menuId = useId();
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
        aria-label={t("Zoom: {percent}", { percent: percent(zoom) })}
        title={t("Zoom: {percent}", { percent: percent(zoom) })}
        aria-haspopup="menu"
        aria-expanded={isOpen}
        aria-controls={isOpen ? menuId : undefined}
        onMouseDown={keepFocus}
        onClick={() => setIsOpen((open) => !open)}
      >
        {percent(zoom)}
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
        <ZoomMenu
          id={menuId}
          zoom={zoom}
          anchor={button.current}
          onZoom={onZoom}
          onClose={() => setIsOpen(false)}
        />
      )}
    </fieldset>
  );
}

interface ZoomMenuProps {
  readonly id: string;
  readonly zoom: number;
  readonly anchor: HTMLElement | null;
  readonly onZoom: (zoom: number) => void;
  readonly onClose: () => void;
}

function ZoomMenu({ id, zoom, anchor, onZoom, onClose }: ZoomMenuProps) {
  const menu = useRef<HTMLDivElement>(null);
  const close = useRef(onClose);
  close.current = onClose;
  // The status bar is at the bottom and cuts off what is larger than itself.
  const [place] = useState(() => {
    const box = anchor?.getBoundingClientRect();
    return {
      bottom: `${box ? window.innerHeight - box.top + 4 : 0}px`,
      right: `${box ? Math.max(8, window.innerWidth - box.right) : 8}px`,
    };
  });

  useEffect(() => {
    const before = document.activeElement;
    const element = menu.current;
    const items = element?.querySelectorAll<HTMLElement>("[role=menuitemradio]") ?? [];
    const first =
      element?.querySelector<HTMLElement>("[aria-checked=true]") ??
      items[ZOOM_STEPS.indexOf(1)] ??
      items[0];
    first?.focus();
    const dismiss = (event: globalThis.MouseEvent) => {
      const { target } = event;
      if (target instanceof Node && !element?.contains(target) && !anchor?.contains(target)) {
        close.current();
      }
    };
    window.addEventListener("mousedown", dismiss);
    return () => {
      window.removeEventListener("mousedown", dismiss);
      // Not if the keyboard has been taken elsewhere meanwhile.
      const isOurs =
        document.activeElement === document.body || element?.contains(document.activeElement);
      const back = before instanceof HTMLElement && before !== document.body ? before : anchor;
      if (isOurs) {
        back?.focus({ preventScroll: true });
      }
    };
  }, [anchor]);

  const onKeyDown = (event: KeyboardEvent) => {
    const items = [...event.currentTarget.querySelectorAll<HTMLElement>("[role=menuitemradio]")];
    const at = items.findIndex((item) => item.contains(document.activeElement));
    const { length } = items;
    const next =
      event.key === "ArrowDown"
        ? (at + 1) % length
        : event.key === "ArrowUp"
          ? (at - 1 + length) % length
          : event.key === "Home"
            ? 0
            : event.key === "End"
              ? length - 1
              : null;
    if (next !== null) {
      event.preventDefault();
      items[next]?.focus();
    } else if (event.key === "Escape" || event.key === "Tab") {
      event.preventDefault();
      event.stopPropagation();
      onClose();
    }
  };

  return (
    <div
      ref={menu}
      id={id}
      className="context-menu zoom-menu"
      role="menu"
      aria-label={t("Zoom")}
      data-testid="zoom-menu"
      style={place}
      onKeyDown={onKeyDown}
    >
      {ZOOM_STEPS.map((step) => (
        <button
          key={step}
          type="button"
          role="menuitemradio"
          aria-checked={step === zoom}
          tabIndex={-1}
          className="ctx-item"
          data-testid={`zoom-${Math.round(step * 100)}`}
          onClick={() => {
            onZoom(step);
            onClose();
          }}
        >
          {percent(step)}
        </button>
      ))}
    </div>
  );
}
