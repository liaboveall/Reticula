export const LOAD_FAILED = "reticula:load-failed";

/**
 * Reports code that could not be loaded by other means than `import()`, such
 * as the script of a worker. `routes-recovery.ts` decides what happens then.
 */
export function reportLoadFailure(): void {
  window.dispatchEvent(new Event(LOAD_FAILED));
}
