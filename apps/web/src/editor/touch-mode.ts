import { touchOverride } from "../hooks.ts";
import { useMedia } from "../ui/media.ts";

/** A touch screen to use the page with, and no mouse, trackpad or pen. */
const TOUCH_ONLY = "(pointer: coarse) and (not (any-pointer: fine))";

/** Whether the editor is for looking only, with fingers. */
export function isTouchMode(): boolean {
  return touchOverride() ?? window.matchMedia(TOUCH_ONLY).matches;
}

/** The same, as it changes with what is connected. A listener is told before the page is. */
export function useTouchMode(beforeChange?: () => void): boolean {
  const matches = useMedia(TOUCH_ONLY, beforeChange);
  return touchOverride() ?? matches;
}
