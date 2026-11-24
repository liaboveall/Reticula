/**
 * The one way to have something read out.
 *
 * While a modal dialog is open the rest of the page is inert, and what happens
 * in a live region there is not read. So every dialog brings a region of its
 * own, and a text goes to the one of the topmost dialog, if there is any.
 */

export type Politeness = "polite" | "assertive";

/** A live region, as far as it is written to. */
export interface LiveRegion {
  /** Where a text is put to be read when nothing else is. */
  readonly polite: { textContent: string | null };
  /** Where a text is put to be read at once. */
  readonly assertive: { textContent: string | null };
}

export interface DialogRegion extends LiveRegion {
  readonly isOpen: () => boolean;
}

/** How long a text waits. What is announced meanwhile takes its place. */
export const ANNOUNCE_DELAY_MS = 150;

export class Announcer {
  #page: LiveRegion | null = null;
  /** In the order they were opened. */
  #dialogs: DialogRegion[] = [];
  readonly #pending = new Map<Politeness, string>();

  /** The region of the page itself. Returns how to take it away. */
  registerPage(region: LiveRegion): () => void {
    this.#page = region;
    return () => {
      if (this.#page === region) {
        this.#page = null;
      }
    };
  }

  /** The region of a dialog that is being opened. Returns how to take it away. */
  registerDialog(region: DialogRegion): () => void {
    this.#dialogs.push(region);
    return () => {
      this.#dialogs = this.#dialogs.filter((one) => one !== region);
    };
  }

  announce(text: string, { politeness = "polite" }: { politeness?: Politeness } = {}): void {
    if (text === "") {
      return;
    }
    const waiting = this.#pending.has(politeness);
    this.#pending.set(politeness, text);
    if (waiting) {
      return;
    }
    // A text that is there already is not read again, unless it was gone for a moment.
    this.#write(politeness, "");
    setTimeout(() => {
      const latest = this.#pending.get(politeness) ?? "";
      this.#pending.delete(politeness);
      this.#write(politeness, latest);
    }, ANNOUNCE_DELAY_MS);
  }

  /** Of which dialog the text is read may have changed since it was announced. */
  #write(politeness: Politeness, text: string): void {
    const region = this.#dialogs.findLast((dialog) => dialog.isOpen()) ?? this.#page;
    if (region) {
      region[politeness].textContent = text;
    }
  }
}

const announcer = new Announcer();

/** Has a text read out that the user would otherwise have to find. */
export function announce(text: string, options?: { politeness?: Politeness }): void {
  announcer.announce(text, options);
}

export function registerPageRegion(region: LiveRegion): () => void {
  return announcer.registerPage(region);
}

export function registerDialogRegion(region: DialogRegion): () => void {
  return announcer.registerDialog(region);
}
