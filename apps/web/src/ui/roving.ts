/** Moving among the items of a list, a palette or a row of tabs with the keyboard. */

export interface RovingLayout {
  /** The items in a row of a palette. */
  readonly columns?: number;
  /** Which arrows go from one item to the next where there is one row or one column. */
  readonly orientation?: "vertical" | "horizontal";
  /** Whether the first item follows the last. */
  readonly wrap?: boolean;
}

/** The item a key goes to from another one, or `null` for a key that does not move. */
export function rovingTarget(
  key: string,
  from: number,
  count: number,
  { columns = 1, orientation = "vertical", wrap = false }: RovingLayout = {},
): number | null {
  if (count === 0) {
    return null;
  }
  const step = (by: number): number => {
    const to = from + by;
    if (wrap) {
      return (to + count) % count;
    }
    return to < 0 || to >= count ? from : to;
  };
  switch (key) {
    case "Home":
      return 0;
    case "End":
      return count - 1;
    case "ArrowDown":
    case "ArrowUp":
      if (columns === 1 && orientation === "horizontal") {
        return null;
      }
      return step(key === "ArrowDown" ? columns : -columns);
    case "ArrowRight":
    case "ArrowLeft":
      if (columns === 1 && orientation === "vertical") {
        return null;
      }
      return step(key === "ArrowRight" ? 1 : -1);
    default:
      return null;
  }
}

/** After how long the next letter begins another word. */
export const TYPE_AHEAD_MS = 500;

/** Finds an item by the beginning of its label, typed letter by letter. */
export class TypeAhead {
  #typed = "";
  #last = Number.NEGATIVE_INFINITY;
  readonly #now: () => number;

  constructor(now: () => number = () => performance.now()) {
    this.#now = now;
  }

  /** Whether a word is being typed, which a space is part of then. */
  get active(): boolean {
    return this.#typed !== "" && this.#now() - this.#last < TYPE_AHEAD_MS;
  }

  /** The item that what has been typed so far means, or `null` if there is none. */
  find(letter: string, labels: readonly string[], from: number): number | null {
    this.#typed = (this.active ? this.#typed : "") + letter.toLocaleLowerCase();
    this.#last = this.#now();
    // The same letter over and over goes through the items it begins.
    const repeated = [...this.#typed].every((one) => one === this.#typed[0]);
    const wanted = repeated ? (this.#typed[0] ?? "") : this.#typed;
    const start = repeated ? from + 1 : from;
    for (let i = 0; i < labels.length; i++) {
      const index = (start + i + labels.length) % labels.length;
      if (labels[index]?.trim().toLocaleLowerCase().startsWith(wanted)) {
        return index;
      }
    }
    return null;
  }
}
