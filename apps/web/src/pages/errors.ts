import { getLocale } from "../i18n/index.ts";

/** Errors the browser words itself, which it does in English whatever the page is in. */
const BROWSER_ERRORS = [TypeError, SyntaxError, DOMException, WebAssembly.CompileError] as const;

/**
 * What an error says, or `fallback`, which is in the language shown. A failed
 * request reads "Failed to fetch" in one browser and "Load failed" in another:
 * nothing to show between Chinese texts.
 */
export function errorText(err: unknown, fallback: string): string {
  if (!(err instanceof Error)) {
    return fallback;
  }
  const isBrowsers = BROWSER_ERRORS.some((kind) => err instanceof kind);
  return isBrowsers && getLocale() !== "en" ? fallback : err.message;
}
