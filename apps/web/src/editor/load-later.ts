import { type ComponentType, lazy } from "react";
import { resolved } from "../ui/resolved.ts";

type Later = typeof import("./later.ts");

let loaded: Later | null = null;
let loading: Promise<Later> | null = null;

/** What is loaded later than the editor itself, if it is there already. */
export function laterNow(): Later | null {
  return loaded;
}

export function loadLater(): Promise<Later> {
  loading ??= import("./later.ts").then(
    (later) => {
      loaded = later;
      return later;
    },
    (error: unknown) => {
      // It is asked for again when it is needed.
      loading = null;
      throw error;
    },
  );
  return loading;
}

/** A component of it. Once it has been loaded, it is there without anything in its place before. */
export function lazyPart<P extends object>(pick: (later: Later) => ComponentType<P>) {
  const part = (later: Later) => ({ default: pick(later) });
  return lazy(() => (loaded ? resolved(part(loaded)) : loadLater().then(part)));
}

/** A name with the keys it goes by, for a tooltip. */
export const withKeys = (text: string, command: string) => loaded?.withKeys(text, command) ?? text;

/** What is said of a part of the editor to whoever points at it. */
export const hint = (part: keyof Later["HINTS"]) => loaded?.HINTS[part]();

/** What the engine has written, in the language shown. */
export const engineText = (text: string) => loaded?.translateEngineText(text) ?? text;

/** Loads it when there is nothing more urgent to do. Never rejects. */
export function loadLaterWhenIdle(then: () => void): void {
  const load = () => void loadLater().then(then, () => {});
  // Safari has no requestIdleCallback.
  if (typeof window.requestIdleCallback === "function") {
    window.requestIdleCallback(load, { timeout: 2_000 });
  } else {
    setTimeout(load, 200);
  }
}
