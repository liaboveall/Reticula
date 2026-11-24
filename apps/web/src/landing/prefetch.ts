import { preloadEditor, preloadEditorCode } from "../preload.ts";

/** Handlers for a link to the editor: whoever reaches for it gets a head start. */
export const PRELOAD_EDITOR_ON_INTENT = {
  onPointerEnter: preloadEditor,
  onPointerDown: preloadEditor,
  onFocus: preloadEditor,
} as const;

export interface PrefetchConditions {
  readonly finePointer: boolean;
  readonly saveData: boolean;
}

/**
 * Whether to fetch code nobody has asked for yet. A fine pointer stands for a
 * device the editor is made for; Save-Data asks for no speculation.
 */
export function mayPrefetch({ finePointer, saveData }: PrefetchConditions): boolean {
  return finePointer && !saveData;
}

function conditions(): PrefetchConditions {
  const { connection } = navigator as Navigator & { connection?: { saveData?: boolean } };
  return {
    finePointer: window.matchMedia("(pointer: fine)").matches,
    saveData: connection?.saveData === true,
  };
}

/**
 * Fetches the editor's JavaScript once the page is loaded and idle. The engine
 * is several times larger and waits for a sign that it will be used. Returns a
 * function that calls it off.
 */
export function prefetchEditorWhenIdle(): () => void {
  if (!mayPrefetch(conditions())) {
    return () => {};
  }
  let idle = 0;
  let timer = 0;
  const schedule = () => {
    // Safari has no requestIdleCallback.
    if (typeof window.requestIdleCallback === "function") {
      idle = window.requestIdleCallback(preloadEditorCode, { timeout: 5_000 });
    } else {
      timer = window.setTimeout(preloadEditorCode, 1_000);
    }
  };
  if (document.readyState === "complete") {
    schedule();
  } else {
    window.addEventListener("load", schedule, { once: true });
  }
  return () => {
    window.removeEventListener("load", schedule);
    if (idle !== 0) {
      window.cancelIdleCallback(idle);
    }
    window.clearTimeout(timer);
  };
}
