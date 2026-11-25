import { tablesLock } from "./delete.ts";

/** How long another tab has to let go before its lock is taken from it. */
const LOCK_PATIENCE_MS = 2_500;

/** What is said on the channel on which tabs ask each other for workbooks. */
type LockMessage =
  // A tab wants the whole workbook, its tables included.
  | { readonly kind: "yield"; readonly docId: string; readonly fromTabId: string }
  // A worker wants the tables alone.
  | { readonly kind: "yieldTables"; readonly docId: string; readonly fromTabId: string };

export interface TablesLockHost {
  readonly docId: string;
  readonly tabId: string;
  readonly locks: LockManager | undefined;
  readonly channel: BroadcastChannel | null;
  /**
   * Another tab asks for the tables, or, `evicted`, has taken them. Resolves
   * once all files are closed.
   */
  onYield(evicted: boolean): Promise<void>;
}

/**
 * The lock on the tables of a workbook. Nothing of them is read or changed
 * without it.
 */
export class TablesLock {
  readonly #host: TablesLockHost;
  #release: (() => void) | null = null;

  constructor(host: TablesLockHost) {
    this.#host = host;
    host.channel?.addEventListener("message", this.#onMessage);
  }

  get held(): boolean {
    return this.#release !== null;
  }

  /**
   * Takes the lock, if nobody has it. With `takeOver` whoever has it is asked to
   * let go, and after a while it is taken from them. Whether it is held then.
   */
  async acquire(takeOver: boolean): Promise<boolean> {
    if (this.held || (await this.#request({ ifAvailable: true }))) {
      return true;
    }
    if (!takeOver) {
      return false;
    }
    this.#say("yieldTables");
    try {
      await this.#request({ signal: AbortSignal.timeout(LOCK_PATIENCE_MS) });
    } catch {
      await this.#request({ steal: true });
    }
    return true;
  }

  release(): void {
    this.#release?.();
    this.#release = null;
  }

  dispose(): void {
    this.#host.channel?.removeEventListener("message", this.#onMessage);
    this.release();
  }

  #say(kind: LockMessage["kind"]): void {
    const { docId, tabId, channel } = this.#host;
    channel?.postMessage({ kind, docId, fromTabId: tabId } satisfies LockMessage);
  }

  readonly #onMessage = (event: MessageEvent<Partial<LockMessage> | null>): void => {
    const { docId, tabId, onYield } = this.#host;
    const { kind, docId: wanted, fromTabId } = event.data ?? {};
    if (
      (kind === "yield" || kind === "yieldTables") &&
      wanted === docId &&
      fromTabId !== tabId &&
      this.held
    ) {
      void onYield(false).finally(() => this.release());
    }
  };

  #request(options: LockOptions): Promise<boolean> {
    const { locks, docId, onYield } = this.#host;
    if (!locks?.request) {
      this.#release = () => {};
      return Promise.resolve(true);
    }
    return new Promise((resolve, reject) => {
      let mine: (() => void) | null = null;
      locks
        .request(tablesLock(docId), options, (lock) => {
          if (!lock) {
            resolve(false);
            return undefined;
          }
          return new Promise<void>((release) => {
            mine = release;
            this.#release = release;
            resolve(true);
          });
        })
        .catch((error: unknown) => {
          if (mine && this.#release === mine) {
            // Another tab took it.
            this.#release = null;
            void onYield(true);
          }
          reject(error);
        });
    });
  }
}
