import { useState, useSyncExternalStore } from "react";

/** Whether a media query matches, as it changes. A listener is told before the page is. */
export function useMedia(query: string, beforeChange?: () => void): boolean {
  const [{ media, subscribe }] = useState(() => {
    const media = window.matchMedia(query);
    const subscribe = (notify: () => void) => {
      const onChange = () => {
        beforeChange?.();
        notify();
      };
      media.addEventListener("change", onChange);
      return () => media.removeEventListener("change", onChange);
    };
    return { media, subscribe };
  });
  return useSyncExternalStore(subscribe, () => media.matches);
}
