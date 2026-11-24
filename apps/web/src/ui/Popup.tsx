import {
  type CSSProperties,
  type KeyboardEvent,
  type ReactNode,
  type RefObject,
  useEffect,
  useLayoutEffect,
  useRef,
} from "react";
import { focusIsLost, isComposing, restoreFocus } from "./focus.ts";
import { type RovingLayout, rovingTarget, TypeAhead } from "./roving.ts";

export type PopupKind = "dialog" | "menu" | "listbox";

/** A point of the window to appear at: below it, or above what is at the bottom. */
export interface PopupPlace {
  readonly x: number;
  readonly y: number;
  readonly above?: boolean;
}

export interface PopupProps {
  readonly id?: string | undefined;
  readonly label: string;
  readonly kind: PopupKind;
  /** The button it belongs to, where pressing is not pressing elsewhere. */
  readonly anchor?: RefObject<HTMLElement | null> | undefined;
  /** Without it, it is placed by its style. */
  readonly at?: PopupPlace | undefined;
  readonly onClose: () => void;
  /** How the arrow keys go from one of the items inside to another. */
  readonly layout?: RovingLayout | undefined;
  readonly className?: string | undefined;
  readonly testId?: string | undefined;
  readonly children: ReactNode;
}

/** What the arrow keys go through. The browser's own Tab order has no part in it. */
const ITEMS = 'button, [role="option"], [role^="menuitem"]';

/** What it keeps free to the edges of the window. */
const MARGIN = 8;

function itemsIn(popup: Element): HTMLElement[] {
  return [...popup.querySelectorAll<HTMLElement>(ITEMS)];
}

/** Goes to the chosen one of the items, else to the first. In a menu of other things, that is none. */
function focusFirst(popup: HTMLElement): void {
  const items = itemsIn(popup);
  const isChoice = !items.some((item) => item.getAttribute("role") === "menuitem");
  const chosen = items.find(
    (item) =>
      isChoice &&
      ["aria-selected", "aria-checked", "aria-pressed"].some(
        (state) => item.getAttribute(state) === "true",
      ),
  );
  (chosen ?? items[0] ?? popup).focus({ preventScroll: true });
  chosen?.scrollIntoView({ block: "nearest" });
}

function placeAt(popup: HTMLElement, { x, y, above = false }: PopupPlace): void {
  const { width, height } = popup.getBoundingClientRect();
  const within = (start: number, size: number, room: number) =>
    Math.max(MARGIN, Math.min(start, room - size - MARGIN));
  popup.style.left = `${within(x, width, window.innerWidth)}px`;
  popup.style.top = `${within(above ? y - height : y, height, window.innerHeight)}px`;
}

/**
 * Something to choose from that appears above the page. The focus moves into
 * it and back to where it was, the arrow keys and letters move inside, and
 * Escape, Tab and pressing elsewhere close it.
 */
export default function Popup({
  id,
  label,
  kind,
  anchor,
  at,
  onClose,
  layout,
  className,
  testId,
  children,
}: PopupProps) {
  const popupRef = useRef<HTMLDivElement>(null);
  const typeAhead = useRef<TypeAhead | null>(null);
  const close = useRef(onClose);
  const restoring = useRef(false);
  const left = useRef<Element | null>(null);
  useLayoutEffect(() => {
    close.current = onClose;
  });

  const { x, y, above } = at ?? {};
  useLayoutEffect(() => {
    if (popupRef.current && x !== undefined && y !== undefined) {
      placeAt(popupRef.current, { x, y, above: above === true });
    }
  }, [x, y, above]);

  useLayoutEffect(() => {
    const popup = popupRef.current;
    if (!popup) {
      return;
    }
    const before = document.activeElement;
    // React's strict mode takes it away for a moment, when the keys may have moved in it.
    if (left.current instanceof HTMLElement && popup.contains(left.current)) {
      left.current.focus({ preventScroll: true });
    } else {
      focusFirst(popup);
    }
    return () => {
      left.current = document.activeElement;
      // Not if the keyboard has been taken elsewhere meanwhile.
      if (focusIsLost() || popup.contains(document.activeElement)) {
        restoring.current = true;
        restoreFocus(before, () => anchor?.current?.focus());
        restoring.current = false;
      }
    };
  }, [anchor]);

  useEffect(() => {
    const popup = popupRef.current;
    if (!popup) {
      return;
    }
    const leaves = (target: EventTarget | null) =>
      target instanceof Node && !popup.contains(target) && !anchor?.current?.contains(target);
    const onPointerDown = (event: PointerEvent) => {
      if (leaves(event.target)) {
        close.current();
      }
    };
    const onFocusOut = (event: FocusEvent) => {
      if (!restoring.current && leaves(event.relatedTarget)) {
        close.current();
      }
    };
    document.addEventListener("pointerdown", onPointerDown, { capture: true });
    popup.addEventListener("focusout", onFocusOut);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown, { capture: true });
      popup.removeEventListener("focusout", onFocusOut);
    };
  }, [anchor]);

  const onTyped = (event: KeyboardEvent, items: HTMLElement[], index: number) => {
    typeAhead.current ??= new TypeAhead();
    const isLetter = event.key.length === 1 && event.key !== " ";
    if (!isLetter && !(event.key === " " && typeAhead.current.active)) {
      return;
    }
    const labels = items.map((item) => item.textContent ?? "");
    const found = typeAhead.current.find(event.key, labels, index);
    // A space that continues no label chooses, as it does otherwise.
    if (isLetter || found !== null) {
      event.preventDefault();
    }
    if (found !== null) {
      items[found]?.focus();
    }
  };

  const onKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    if (isComposing(event.nativeEvent)) {
      return;
    }
    if (event.key === "Escape") {
      // A dialog around it stays open.
      event.preventDefault();
      close.current();
      return;
    }
    if (event.key === "Tab") {
      if (kind === "menu") {
        event.preventDefault();
      } else {
        // From here the browser goes on to what comes after the button, or before it.
        anchor?.current?.focus();
      }
      close.current();
      return;
    }
    if (event.ctrlKey || event.metaKey || event.altKey) {
      return;
    }
    const items = itemsIn(event.currentTarget);
    const index = items.findIndex((item) => item.contains(document.activeElement));
    const to = rovingTarget(event.key, Math.max(0, index), items.length, layout);
    if (to !== null) {
      event.preventDefault();
      items[to]?.focus();
    } else if (kind !== "dialog") {
      onTyped(event, items, index);
    }
  };

  const style: CSSProperties | undefined = at && { position: "fixed", left: 0, top: 0 };
  const attributes = { id, role: kind, "aria-label": label, "data-testid": testId, onKeyDown };
  return (
    <div
      {...attributes}
      ref={popupRef}
      className={className ? `popover ${className}` : "popover"}
      style={style}
      tabIndex={-1}
    >
      {children}
    </div>
  );
}
