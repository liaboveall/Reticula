/**
 * The one way to have something read out.
 *
 * While a modal dialog is open the rest of the page is inert, and what happens
 * in a live region there is not read. So every dialog brings a region of its
 * own, and a text goes to the one of the topmost dialog, if there is any.
 */

import { t } from "../i18n/index.ts";

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

export interface AnnounceOptions {
  readonly politeness?: Politeness;
  /**
   * Whether it says where the selection is, which comes after what else there is
   * to say and does not take its place.
   */
  readonly isPlace?: boolean;
}

interface Pending {
  event: string;
  place: string;
  /** Where the place was to be read. */
  region: LiveRegion | null;
}

function joined({ event, place }: Pending): string {
  return event === "" || place === ""
    ? event + place
    : t("{first}. {second}", { first: event.replace(/[.。]$/, ""), second: place });
}

/** How long a text waits. What is announced meanwhile takes its place. */
export const ANNOUNCE_DELAY_MS = 150;

export class Announcer {
  #page: LiveRegion | null = null;
  /** In the order they were opened. */
  #dialogs: DialogRegion[] = [];
  readonly #pending = new Map<Politeness, Pending>();

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

  announce(text: string, { politeness = "polite", isPlace = false }: AnnounceOptions = {}): void {
    if (text === "") {
      return;
    }
    const waiting = this.#pending.get(politeness);
    const pending = waiting ?? { event: "", place: "", region: null };
    pending[isPlace ? "place" : "event"] = text;
    if (isPlace) {
      pending.region = this.#region();
    }
    if (waiting) {
      return;
    }
    this.#pending.set(politeness, pending);
    // A text that is there already is not read again, unless it was gone for a moment.
    this.#write(politeness, "");
    setTimeout(() => {
      this.#pending.delete(politeness);
      // Where the selection is does not matter to a dialog that has been opened since.
      if (pending.region !== this.#region()) {
        pending.place = "";
      }
      this.#write(politeness, joined(pending));
    }, ANNOUNCE_DELAY_MS);
  }

  #region(): LiveRegion | null {
    return this.#dialogs.findLast((dialog) => dialog.isOpen()) ?? this.#page;
  }

  /** Of which dialog the text is read may have changed since it was announced. */
  #write(politeness: Politeness, text: string): void {
    const region = this.#region();
    if (region) {
      region[politeness].textContent = text;
    }
  }
}

const announcer = new Announcer();

/** Has a text read out that the user would otherwise have to find. */
export function announce(text: string, options?: AnnounceOptions): void {
  announcer.announce(text, options);
}

export function registerPageRegion(region: LiveRegion): () => void {
  return announcer.registerPage(region);
}

export function registerDialogRegion(region: DialogRegion): () => void {
  return announcer.registerDialog(region);
}
