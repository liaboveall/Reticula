/** What was typed into a cell whose row or column is not known yet. */
export interface HeldCommit {
  readonly sheet: number;
  readonly row: number;
  readonly col: number;
  readonly raw: string;
}

export interface CellIds {
  readonly rowId: string;
  readonly colId: string;
}

export interface HeldCommitsHost {
  /** The row and column of a cell of the sheet in view, once the page with it has come. */
  idsOf(row: number, col: number): CellIds | null;
  sheetId(): number;
  /** Returns `false` if nothing may be changed. */
  commit(held: HeldCommit, ids: CellIds): boolean;
  /** Called for what could not be committed in time. */
  fail(held: HeldCommit): void;
}

/** How long the page with a cell may take to come. */
export const HOLD_MS = 5_000;

/**
 * Keeps what was typed into cells until it is known which they are: right after
 * another sheet or a far part of one has come into view, that takes a moment.
 */
export class HeldCommits {
  readonly #host: HeldCommitsHost;
  #held: (HeldCommit & { readonly timer: ReturnType<typeof setTimeout> })[] = [];

  constructor(host: HeldCommitsHost) {
    this.#host = host;
  }

  hold(commit: HeldCommit): void {
    const timer = setTimeout(() => this.#giveUp(timer), HOLD_MS);
    this.#held.push({ ...commit, timer });
    this.retry();
  }

  /** To be called when a page has come. */
  retry(): void {
    if (this.#held.length === 0) {
      return;
    }
    const held = this.#held;
    this.#held = [];
    for (const one of held) {
      const ids = one.sheet === this.#host.sheetId() ? this.#host.idsOf(one.row, one.col) : null;
      if (ids) {
        clearTimeout(one.timer);
        if (!this.#host.commit(one, ids)) {
          this.#host.fail(one);
        }
      } else {
        this.#held.push(one);
      }
    }
  }

  /** Gives up on everything, because another workbook is being opened. */
  clear(): void {
    const held = this.#held;
    this.#held = [];
    for (const one of held) {
      clearTimeout(one.timer);
      this.#host.fail(one);
    }
  }

  #giveUp(timer: ReturnType<typeof setTimeout>): void {
    const late = this.#held.find((one) => one.timer === timer);
    if (late) {
      this.#held = this.#held.filter((one) => one !== late);
      this.#host.fail(late);
    }
  }
}
