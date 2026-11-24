import { getLocale } from "./i18n/index.ts";
import type { AppRoute } from "./routes.ts";

declare global {
  interface Window {
    /**
     * Preloads the code of a page, its Chinese texts, or what the engine's
     * worker needs. The build puts it into index.html
     * (`crates/xtask/src/web/bundle.rs`), together with the names of the files;
     * a development server does without it.
     */
    __reticulaPreload?: (set: AppRoute | `zh:${AppRoute}` | "engine") => void;
  }
}

/** Fetches the editor's JavaScript, and its texts, without running any of it. */
export function preloadEditorCode(): void {
  window.__reticulaPreload?.("app");
  if (getLocale() !== "en") {
    window.__reticulaPreload?.("zh:app");
  }
}

/** Fetches everything the editor's first screen needs: its code, the worker's, and the engine. */
export function preloadEditor(): void {
  preloadEditorCode();
  window.__reticulaPreload?.("engine");
}
