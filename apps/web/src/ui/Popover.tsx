import {
  type KeyboardEvent,
  type MouseEvent,
  type ReactNode,
  type RefObject,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
} from "react";
import { focusIsLost, isComposing } from "./focus.ts";
import { type RovingLayout, rovingTarget } from "./roving.ts";

/** What the button that opens a popover has to have. */
export interface PopoverTriggerProps {
  readonly ref: RefObject<HTMLButtonElement | null>;
  readonly id: string;
  readonly "aria-haspopup": "dialog" | "menu" | "listbox";
  readonly "aria-expanded": boolean;
  readonly "aria-controls": string | undefined;
  readonly onClick: (event: MouseEvent) => void;
  readonly onKeyDown: (event: KeyboardEvent) => void;
}

export interface PopoverProps {
  readonly label: string;
  readonly open: boolean;
  readonly onOpenChange: (open: boolean) => void;
  readonly trigger: (props: PopoverTriggerProps) => ReactNode;
  readonly kind?: "dialog" | "menu" | "listbox";
  /** How the arrow keys go from one of the buttons inside to another. */
  readonly layout?: RovingLayout;
  /** For other keys. Those it has a use for it prevents. */
  readonly onItemKeyDown?: (event: KeyboardEvent, items: HTMLElement[], index: number) => void;
  readonly wrapClassName?: string;
  readonly className?: string;
  readonly testId?: string;
  readonly children: ReactNode;
}

/** What the arrow keys go through. The browser's own Tab order has no part in it. */
const ITEMS = 'button, [role="option"], [role="menuitem"]';

function itemsIn(popup: Element): HTMLElement[] {
  return [...popup.querySelectorAll<HTMLElement>(ITEMS)];
}

/** Goes to the chosen one of the items, else to the first. */
function focusFirst(popup: HTMLElement): void {
  const items = itemsIn(popup);
  const chosen = items.find((item) =>
    ["aria-selected", "aria-checked", "aria-pressed"].some(
      (state) => item.getAttribute(state) === "true",
    ),
  );
  (chosen ?? items[0] ?? popup).focus();
}

/** Takes the focus when it appears, and gives it back unless it went elsewhere. */
function Popup({
  triggerRef,
  children,
  ...rest
}: {
  readonly triggerRef: RefObject<HTMLButtonElement | null>;
  readonly id: string;
  readonly role: string;
  readonly "aria-label": string;
  readonly className: string | undefined;
  readonly "data-testid": string | undefined;
  readonly onKeyDown: (event: KeyboardEvent<HTMLElement>) => void;
  readonly children: ReactNode;
}) {
  const popupRef = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const popup = popupRef.current;
    if (!popup) {
      return;
    }
    focusFirst(popup);
    return () => {
      if (focusIsLost() || popup.contains(document.activeElement)) {
        triggerRef.current?.focus();
      }
    };
  }, [triggerRef]);
  return (
    <div {...rest} ref={popupRef} tabIndex={-1}>
      {children}
    </div>
  );
}

/**
 * Something to choose from, below the button it belongs to. The focus moves
 * into it and back, the arrow keys move inside, and Escape, Tab and pressing
 * elsewhere close it.
 */
export function Popover({
  label,
  open,
  onOpenChange,
  trigger,
  kind = "dialog",
  layout,
  onItemKeyDown,
  wrapClassName,
  className,
  testId,
  children,
}: PopoverProps) {
  const id = useId();
  const wrapRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const close = useRef(() => {});
  useLayoutEffect(() => {
    close.current = () => onOpenChange(false);
  });

  useEffect(() => {
    const wrap = wrapRef.current;
    if (!open || !wrap) {
      return;
    }
    const leaves = (target: EventTarget | null) => target instanceof Node && !wrap.contains(target);
    const onPointerDown = (event: PointerEvent) => {
      if (leaves(event.target)) {
        close.current();
      }
    };
    const onFocusOut = (event: FocusEvent) => {
      if (leaves(event.relatedTarget)) {
        close.current();
      }
    };
    document.addEventListener("pointerdown", onPointerDown, { capture: true });
    wrap.addEventListener("focusout", onFocusOut);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown, { capture: true });
      wrap.removeEventListener("focusout", onFocusOut);
    };
  }, [open]);

  const onPopupKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    if (isComposing(event.nativeEvent)) {
      return;
    }
    if (event.key === "Escape") {
      // A dialog around it stays open.
      event.preventDefault();
      triggerRef.current?.focus();
      onOpenChange(false);
      return;
    }
    if (event.key === "Tab") {
      // From here the browser goes on to what comes after the button, or before it.
      triggerRef.current?.focus();
      onOpenChange(false);
      return;
    }
    const items = itemsIn(event.currentTarget);
    const index = items.findIndex((item) => item.contains(document.activeElement));
    onItemKeyDown?.(event, items, index);
    if (event.defaultPrevented || event.ctrlKey || event.metaKey || event.altKey) {
      return;
    }
    const to = rovingTarget(event.key, Math.max(0, index), items.length, layout);
    if (to !== null) {
      event.preventDefault();
      items[to]?.focus();
    }
  };

  return (
    <div ref={wrapRef} className={wrapClassName ? `popover-wrap ${wrapClassName}` : "popover-wrap"}>
      {trigger({
        ref: triggerRef,
        id: `${id}-trigger`,
        "aria-haspopup": kind,
        "aria-expanded": open,
        "aria-controls": open ? id : undefined,
        onClick: (event) => {
          if (event.currentTarget.getAttribute("aria-disabled") !== "true") {
            onOpenChange(!open);
          }
        },
        onKeyDown: (event) => {
          const opens = event.key === "ArrowDown" || event.key === "ArrowUp";
          if (opens && !open && event.currentTarget.getAttribute("aria-disabled") !== "true") {
            event.preventDefault();
            onOpenChange(true);
          }
        },
      })}
      {open && (
        <Popup
          triggerRef={triggerRef}
          id={id}
          role={kind}
          aria-label={label}
          className={className ? `popover ${className}` : "popover"}
          data-testid={testId}
          onKeyDown={onPopupKeyDown}
        >
          {children}
        </Popup>
      )}
    </div>
  );
}
