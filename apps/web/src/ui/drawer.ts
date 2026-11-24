import { type RefObject, useLayoutEffect, useRef } from "react";
import { focusIsLost, isComposing, keepFocusIn, restoreFocus } from "./focus.ts";

export interface DrawerOptions {
  readonly open: boolean;
  /** What has the focus when it opens. */
  readonly initialFocus: RefObject<HTMLElement | null>;
  /** What had the focus when it was opened (`useOpener`). */
  readonly opener: RefObject<Element | null>;
  /** Takes the focus to what the drawer is beside. */
  readonly onLeave: () => void;
}

/**
 * The keyboard in a panel beside the grid, which stays open while the grid is
 * worked in: the focus moves in when it opens, an Escape takes it to the grid,
 * and when the panel is closed from inside it goes back to where it was. Only
 * a button or a command closes it. Returns a ref for the panel.
 */
export function useDrawer({
  open,
  initialFocus,
  opener,
  onLeave,
}: DrawerOptions): RefObject<HTMLElement | null> {
  const drawerRef = useRef<HTMLElement>(null);
  const leave = useRef(onLeave);
  useLayoutEffect(() => {
    leave.current = onLeave;
  });

  useLayoutEffect(() => {
    const drawer = drawerRef.current;
    if (!open || !drawer) {
      return;
    }
    initialFocus.current?.focus();
    const letFocusGo = keepFocusIn(drawer);
    // Once everything inside has had its chance to use the key.
    const onKeyDown = (event: KeyboardEvent) => {
      const inside = event.target instanceof Node && drawer.contains(event.target);
      if (inside && event.key === "Escape" && !event.defaultPrevented && !isComposing(event)) {
        event.preventDefault();
        leave.current();
      }
    };
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("keydown", onKeyDown);
      letFocusGo();
      if (focusIsLost() || drawer.contains(document.activeElement)) {
        restoreFocus(opener.current, leave.current);
      }
    };
  }, [open, initialFocus, opener]);

  return drawerRef;
}
