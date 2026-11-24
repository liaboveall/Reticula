/** Where the keyboard is, where it goes, and how it does not get lost. */

import { type RefObject, useEffect, useRef } from "react";

const FOCUSABLE =
  'a[href], button, input:not([type="hidden"]), select, textarea, summary, [contenteditable="true"], [tabindex]';

function isShown(element: Element): boolean {
  return element.getClientRects().length > 0;
}

/** What can be reached with the Tab key in a part of the page, in order. */
export function tabbablesIn(container: Element): HTMLElement[] {
  return [...container.querySelectorAll<HTMLElement>(FOCUSABLE)].filter(
    (element) => element.tabIndex >= 0 && !element.matches(":disabled") && isShown(element),
  );
}

/** Whether nothing in particular has the focus, as after the element that had it was removed. */
export function focusIsLost(): boolean {
  const { activeElement, body } = document;
  return activeElement === null || activeElement === body;
}

/**
 * Puts the focus back to where it was before something opened, or elsewhere if
 * that is gone or does not take it any more.
 */
export function restoreFocus(target: Element | null, fallback?: () => void): void {
  if (target instanceof HTMLElement && target !== document.body && target.isConnected) {
    target.focus({ preventScroll: true });
    if (document.activeElement === target) {
      return;
    }
  }
  fallback?.();
}

/**
 * Keeps the focus in a part of the page when the element that has it is taken
 * away: it goes to what is in its place now, such as the same button of the
 * next row of a list. Returns how to stop.
 */
export function keepFocusIn(container: HTMLElement): () => void {
  let focused: HTMLElement | null = null;
  let position = 0;

  const locate = () => {
    if (focused?.isConnected) {
      position = Math.max(0, tabbablesIn(container).indexOf(focused));
    }
  };
  const onFocusIn = (event: FocusEvent) => {
    focused = event.target instanceof HTMLElement ? event.target : null;
    locate();
  };
  const onFocusOut = () => {
    const left = focused;
    // Some browsers report the removal of an element like this, some do not.
    queueMicrotask(() => {
      if (left?.isConnected && focused === left && document.activeElement !== left) {
        focused = null;
      }
    });
  };
  const observer = new MutationObserver(() => {
    if (!focused) {
      return;
    }
    if (focused.isConnected) {
      locate();
      return;
    }
    focused = null;
    if (focusIsLost()) {
      const now = tabbablesIn(container);
      (now[Math.min(position, now.length - 1)] ?? container).focus();
    }
  });

  container.addEventListener("focusin", onFocusIn);
  container.addEventListener("focusout", onFocusOut);
  observer.observe(container, { childList: true, subtree: true });
  const { activeElement } = document;
  if (activeElement instanceof HTMLElement && container.contains(activeElement)) {
    focused = activeElement;
    locate();
  }
  return () => {
    observer.disconnect();
    container.removeEventListener("focusin", onFocusIn);
    container.removeEventListener("focusout", onFocusOut);
  };
}

/** `keepFocusIn` for as long as a component is there. Returns a ref for the part of the page. */
export function useKeptFocus(): RefObject<HTMLElement | null> {
  const ref = useRef<HTMLElement>(null);
  useEffect(() => (ref.current ? keepFocusIn(ref.current) : undefined), []);
  return ref;
}

/** Whether an input method is at work, whose keys are its own. */
export function isComposing(event: {
  readonly isComposing: boolean;
  readonly keyCode: number;
}): boolean {
  return event.isComposing || event.keyCode === 229;
}
