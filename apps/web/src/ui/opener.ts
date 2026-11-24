// By itself, because it is the one thing about the focus the editor starts with.

import { type RefObject, useRef } from "react";

/**
 * What had the focus when `open` last became true, for `restoreFocus`. It is
 * called by whoever opens something: what is opened may take a while to load,
 * and the element may have been taken away by then.
 */
export function useOpener(open: boolean): RefObject<Element | null> {
  const opener = useRef<Element | null>(null);
  const wasOpen = useRef(false);
  if (open !== wasOpen.current) {
    wasOpen.current = open;
    if (open) {
      opener.current = document.activeElement;
    }
  }
  return opener;
}
