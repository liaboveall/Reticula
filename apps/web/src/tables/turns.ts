/** How long jobs run before whatever else there is gets its turn, in milliseconds. */
export const JOB_SLICE_MS = 8;

export interface TurnOptions {
  /** Sends pages for a while. Whether there are more. */
  readonly pages: () => boolean;
  readonly sliceMs?: number;
  readonly now?: () => number;
  /** Has a function run after what the worker was sent in the meantime. */
  readonly defer?: (run: () => void) => void;
}

/**
 * Takes turns between pages and jobs. A job is a function that waits for
 * `pause` after everything it does that takes a while: that goes on at once as
 * long as the slice lasts, and then after the messages that came and the pages
 * that are due.
 */
export class Turns {
  readonly #options: TurnOptions;
  readonly #now: () => number;
  readonly #defer: (run: () => void) => void;
  readonly #waiting: (() => void)[] = [];
  #until = 0;
  #scheduled = false;

  constructor(options: TurnOptions) {
    this.#options = options;
    this.#now = options.now ?? (() => performance.now());
    this.#defer = options.defer ?? viaChannel();
  }

  /** Says that pages are due. */
  wake(): void {
    if (!this.#scheduled) {
      this.#scheduled = true;
      this.#defer(() => this.#turn());
    }
  }

  /** Ends a piece of a job. Throws what the signal was aborted with. */
  async pause(signal: AbortSignal): Promise<void> {
    if (this.#now() >= this.#until) {
      await new Promise<void>((resolve) => {
        this.#waiting.push(resolve);
        this.wake();
      });
    }
    signal.throwIfAborted();
  }

  #turn(): void {
    this.#scheduled = false;
    this.#until = 0;
    if (this.#options.pages()) {
      this.wake();
      return;
    }
    const next = this.#waiting.shift();
    if (next) {
      this.#until = this.#now() + (this.#options.sliceMs ?? JOB_SLICE_MS);
      next();
    }
    if (this.#waiting.length > 0) {
      this.wake();
    }
  }
}

/** Timers are held back to 4 ms when they follow each other; messages are not. */
function viaChannel(): (run: () => void) => void {
  const { port1, port2 } = new MessageChannel();
  let next: (() => void) | null = null;
  port1.onmessage = () => {
    const run = next;
    next = null;
    run?.();
  };
  return (run) => {
    next = run;
    port2.postMessage(0);
  };
}
