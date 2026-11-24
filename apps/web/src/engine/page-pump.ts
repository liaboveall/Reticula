import type { EngineEvent } from "@reticula/protocol";

/** An engine that viewport pages come from: the live one, a draft's or the history player. */
export interface PageSource {
  takePage(): Uint8Array | undefined;
  /** An event the page that was taken last depends on, such as new styles. */
  takeAuxEvent(): EngineEvent | undefined;
}

export type PagePost = (message: EngineEvent | ArrayBuffer, transfer?: Transferable[]) => void;

/** How long pages are packed before commands get their turn. */
export const PAGE_SLICE_BUDGET_MS = 4;

/**
 * Posts due pages one by one, so that the first is on its way before the last
 * is packed, until there are none left or the budget is spent. Returns whether
 * there may be more.
 */
export function pumpPages(
  source: PageSource,
  post: PagePost,
  budgetMs: number = PAGE_SLICE_BUDGET_MS,
  now: () => number = () => performance.now(),
): boolean {
  const started = now();
  for (;;) {
    const page = source.takePage();
    for (let aux = source.takeAuxEvent(); aux; aux = source.takeAuxEvent()) {
      post(aux);
    }
    if (!page) {
      return false;
    }
    const buffer = page.buffer as ArrayBuffer;
    post(buffer, [buffer]);
    if (now() - started >= budgetMs) {
      return true;
    }
  }
}
