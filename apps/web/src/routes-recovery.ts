/**
 * What to do when a part of the app's code cannot be loaded. The usual cause
 * is a deploy: a page opened before it names chunks the server no longer has,
 * and loading the address again gets the new names.
 */

import { LOAD_FAILED } from "./load-failure.ts";

/** The builds that have already been left by an automatic reload in this tab. */
const RELOADED_BUILDS_KEY = "reticula:reloaded-builds";

/** The parts of the browser recovery needs, replaceable in tests. */
export interface RecoveryHost {
  /** Names the running build: the address of its entry chunk, which is hashed. */
  readonly build: string;
  readonly storage: Pick<Storage, "getItem" | "setItem"> | null;
  isOnline(): boolean;
  /** Whether the browser is on its way to another document. */
  isLeaving(): boolean;
  /** Loads an address from the server, or the current one again for `null`. */
  load(target: string | null): void;
}

/**
 * - `ok`: nothing is missing;
 * - `reloading`: the page is about to be replaced;
 * - `stale`: reloading did not help, so it is up to the user;
 * - `offline`: there is no network to reload from.
 */
export type RecoveryState = "ok" | "reloading" | "stale" | "offline";

export class LoadRecovery {
  readonly #host: RecoveryHost;
  readonly #failures = new WeakSet<object>();
  readonly #listeners = new Set<() => void>();
  #state: RecoveryState = "ok";

  constructor(host: RecoveryHost) {
    this.#host = host;
  }

  /** Remembers that an error means "this code could not be loaded". */
  noteFailure(error: unknown): void {
    if (typeof error === "object" && error !== null) {
      this.#failures.add(error);
    }
  }

  isLoadFailure(error: unknown): boolean {
    return typeof error === "object" && error !== null && this.#failures.has(error);
  }

  /**
   * Loads `target` (or the current address) afresh, once for every build: a
   * build that is still incomplete after that is reported instead.
   */
  recover(target: string | null = null): void {
    // Leaving a page aborts what it is still loading. Firefox and Safari report
    // that as a failure, and a reload would take the place of the navigation.
    if (this.#state === "reloading" || this.#host.isLeaving()) {
      return;
    }
    if (!this.#host.isOnline()) {
      this.#set("offline");
    } else if (this.#claimReload()) {
      this.#set("reloading");
      this.#host.load(target);
    } else {
      this.#set("stale");
    }
  }

  /** Called when the browser brings the page back that was left for another document. */
  restored(): void {
    this.#set("ok");
  }

  /** Called when a page has loaded after all. */
  clear(): void {
    if (this.#state !== "reloading") {
      this.#set("ok");
    }
  }

  readonly state = (): RecoveryState => this.#state;

  readonly subscribe = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  };

  #claimReload(): boolean {
    const { build, storage } = this.#host;
    try {
      const reloaded = storage?.getItem(RELOADED_BUILDS_KEY)?.split(" ") ?? [];
      if (!storage || reloaded.includes(build)) {
        return false;
      }
      storage.setItem(RELOADED_BUILDS_KEY, [...reloaded, build].join(" "));
      return true;
    } catch {
      // Without a note that outlives the reload, reloading could go on forever.
      return false;
    }
  }

  #set(state: RecoveryState): void {
    if (this.#state !== state) {
      this.#state = state;
      for (const listener of this.#listeners) {
        listener();
      }
    }
  }
}

function sessionStorageOrNull(): Storage | null {
  try {
    return window.sessionStorage;
  } catch {
    return null;
  }
}

/**
 * How long after a navigation has begun it is taken to be under way. It may
 * have come to nothing: a download, or a user who chose to stay.
 */
const LEAVING_MS = 10_000;

/** Recovery for the page that runs the build whose entry chunk is at `build`. */
export function startRecovery(build: string): LoadRecovery {
  let leavingUntil = 0;
  window.addEventListener("beforeunload", () => {
    leavingUntil = performance.now() + LEAVING_MS;
  });
  window.addEventListener("pagehide", () => {
    leavingUntil = Number.POSITIVE_INFINITY;
  });
  const recovery = new LoadRecovery({
    build,
    storage: sessionStorageOrNull(),
    isOnline: () => navigator.onLine,
    isLeaving: () => performance.now() < leavingUntil,
    load: (target) => {
      if (target === null) {
        window.location.reload();
      } else {
        window.location.assign(target);
      }
    },
  });
  window.addEventListener("vite:preloadError", (event) => recovery.noteFailure(event.payload));
  window.addEventListener(LOAD_FAILED, () => recovery.recover());
  window.addEventListener("pageshow", (event) => {
    leavingUntil = 0;
    if (event.persisted) {
      recovery.restored();
    }
  });
  // A failure that nobody handles: there is no page or error boundary to
  // decide what becomes of it. Code that can do without what it asked for
  // catches the failure itself, and then nothing happens here.
  window.addEventListener("unhandledrejection", (event) => {
    if (recovery.isLoadFailure(event.reason)) {
      recovery.recover();
    }
  });
  return recovery;
}
