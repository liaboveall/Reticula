/**
 * Service Worker registration and multi-tab coordinated update client
 * (`14-m1b.md` 7.4 & D-019).
 *
 * Protocol over `BroadcastChannel("reticula:sw")`:
 * 1. When any tab sees a `waiting` Service Worker (while already controlled), it
 *    notifies sibling tabs via `{ kind: "updateReady" }` so every tab shows the
 *    non-intrusive update banner without calling `skipWaiting()` automatically.
 * 2. When the user confirms the update in any tab, that initiator discovers live
 *    sibling tabs (`discoverPeers` / `peerPresent`), broadcasts `drainForUpdate`,
 *    drains its own OPFS handles & Web Locks, waits for every peer to reply
 *    `drainedForUpdate`, and then posts `{ kind: "SKIP_WAITING" }` to the waiting
 *    Service Worker.
 * 3. When the new Service Worker activates and `controllerchange` fires, every
 *    previously controlled tab reloads cleanly onto the new build.
 */

import type { SwStatus } from "./hooks.ts";

export type { SwStatus };

export interface SwClientOptions {
  readonly tabId: string;
  readonly onUpdateReady: (ready: boolean) => void;
  readonly onDrainForUpdate: () => Promise<void>;
}

export interface SwClientHandle {
  applyUpdate(): Promise<void>;
  checkForUpdate(): Promise<void>;
  status(): Promise<SwStatus>;
  dispose(): void;
}

type SwChannelMessage =
  | { readonly kind: "updateReady"; readonly fromTabId: string }
  | { readonly kind: "discoverPeers"; readonly updateId: string; readonly fromTabId: string }
  | { readonly kind: "peerPresent"; readonly updateId: string; readonly tabId: string }
  | { readonly kind: "drainForUpdate"; readonly updateId: string; readonly fromTabId: string }
  | { readonly kind: "drainedForUpdate"; readonly updateId: string; readonly tabId: string };

const CHANNEL_NAME = "reticula:sw";
const DISCOVER_WINDOW_MS = 150;
const DRAIN_TIMEOUT_MS = 2_500;
const VERSION_TIMEOUT_MS = 1_500;

function queryWorkerBuildId(worker: ServiceWorker): Promise<string | null> {
  return new Promise((resolve) => {
    const channel = new MessageChannel();
    const timer = window.setTimeout(() => resolve(null), VERSION_TIMEOUT_MS);
    channel.port1.onmessage = (event: MessageEvent<{ kind?: string; buildId?: string }>) => {
      window.clearTimeout(timer);
      resolve(event.data?.kind === "VERSION" ? (event.data.buildId ?? null) : null);
    };
    try {
      worker.postMessage({ kind: "GET_VERSION" }, [channel.port2]);
    } catch {
      window.clearTimeout(timer);
      resolve(null);
    }
  });
}

export function createSwClient(options: SwClientOptions): SwClientHandle {
  const swContainer =
    typeof navigator !== "undefined" && "serviceWorker" in navigator
      ? navigator.serviceWorker
      : null;

  if (!swContainer || !import.meta.env.PROD) {
    return {
      applyUpdate: async () => {},
      checkForUpdate: async () => {},
      status: async () => ({
        supported: Boolean(swContainer),
        registered: false,
        controlled: false,
        waiting: false,
        buildId: null,
      }),
      dispose: () => {},
    };
  }

  let disposed = false;
  let registration: ServiceWorkerRegistration | null = null;
  let hadController = Boolean(swContainer.controller);
  let reloaded = false;
  let updating = false;

  const channel =
    typeof BroadcastChannel !== "undefined" ? new BroadcastChannel(CHANNEL_NAME) : null;

  // Pending coordination sets when THIS tab initiates an update.
  let activeUpdateId: string | null = null;
  const discoveredPeers = new Set<string>();
  const drainedPeers = new Set<string>();
  let onPeerDrained: (() => void) | null = null;

  const notifyWaiting = () => {
    if (disposed) {
      return;
    }
    options.onUpdateReady(true);
    channel?.postMessage({
      kind: "updateReady",
      fromTabId: options.tabId,
    } satisfies SwChannelMessage);
  };

  const watchRegistration = (reg: ServiceWorkerRegistration) => {
    registration = reg;
    if (reg.waiting && swContainer.controller) {
      notifyWaiting();
    }
    reg.addEventListener("updatefound", () => {
      const installing = reg.installing;
      if (!installing) {
        return;
      }
      installing.addEventListener("statechange", () => {
        if (installing.state === "installed" && swContainer.controller) {
          notifyWaiting();
        }
      });
    });
  };

  const onControllerChange = () => {
    if (disposed || reloaded) {
      return;
    }
    if (!hadController) {
      // First-ever activation claiming this uncontrolled page — do not reload.
      hadController = true;
      return;
    }
    reloaded = true;
    window.location.reload();
  };
  swContainer.addEventListener("controllerchange", onControllerChange);

  if (channel) {
    channel.onmessage = (event: MessageEvent<SwChannelMessage>) => {
      if (disposed) {
        return;
      }
      const msg = event.data;
      if (!msg || typeof msg !== "object") {
        return;
      }
      switch (msg.kind) {
        case "updateReady":
          if (msg.fromTabId !== options.tabId) {
            options.onUpdateReady(true);
            void registration?.update().catch(() => {});
          }
          break;
        case "discoverPeers":
          if (msg.fromTabId !== options.tabId) {
            channel.postMessage({
              kind: "peerPresent",
              updateId: msg.updateId,
              tabId: options.tabId,
            } satisfies SwChannelMessage);
          }
          break;
        case "peerPresent":
          if (msg.updateId === activeUpdateId && msg.tabId !== options.tabId) {
            discoveredPeers.add(msg.tabId);
          }
          break;
        case "drainForUpdate":
          if (msg.fromTabId !== options.tabId) {
            hadController = true;
            void options
              .onDrainForUpdate()
              .catch(() => {})
              .finally(() => {
                if (!disposed) {
                  channel.postMessage({
                    kind: "drainedForUpdate",
                    updateId: msg.updateId,
                    tabId: options.tabId,
                  } satisfies SwChannelMessage);
                }
              });
          }
          break;
        case "drainedForUpdate":
          if (msg.updateId === activeUpdateId) {
            drainedPeers.add(msg.tabId);
            onPeerDrained?.();
          }
          break;
      }
    };
  }

  const readyPromise = swContainer
    .register("/sw.js", { scope: "/" })
    .then((reg) => {
      if (!disposed) {
        watchRegistration(reg);
      }
      return reg;
    })
    .catch(() => null);

  const checkForUpdate = async (): Promise<void> => {
    const reg = registration ?? (await readyPromise);
    if (!reg || disposed) {
      return;
    }
    try {
      await reg.update();
    } catch {
      // Offline — ignore update check errors.
    }
    if (reg.waiting && swContainer.controller) {
      notifyWaiting();
    }
  };

  const applyUpdate = async (): Promise<void> => {
    if (updating || disposed) {
      return;
    }
    const reg = registration ?? (await readyPromise);
    if (!reg) {
      return;
    }
    let waiting = reg.waiting;
    if (!waiting) {
      try {
        await reg.update();
      } catch {
        // Ignore offline update check error.
      }
      waiting = reg.waiting;
    }
    if (!waiting) {
      return;
    }

    updating = true;
    hadController = true;
    const updateId = `${options.tabId}-${Date.now().toString(36)}`;
    activeUpdateId = updateId;
    discoveredPeers.clear();
    drainedPeers.clear();

    if (channel) {
      channel.postMessage({
        kind: "discoverPeers",
        updateId,
        fromTabId: options.tabId,
      } satisfies SwChannelMessage);
      await new Promise<void>((r) => window.setTimeout(r, DISCOVER_WINDOW_MS));
    }

    // Drain all sibling tabs AND this tab concurrently so every OPFS handle and
    // Web Lock closes cleanly before the new Service Worker takes control.
    const peersWait =
      channel && discoveredPeers.size > 0
        ? new Promise<void>((resolve) => {
            const finish = () => {
              window.clearTimeout(cap);
              onPeerDrained = null;
              resolve();
            };
            const cap = window.setTimeout(finish, DRAIN_TIMEOUT_MS);
            onPeerDrained = () => {
              for (const id of discoveredPeers) {
                if (!drainedPeers.has(id)) {
                  return;
                }
              }
              finish();
            };
            channel.postMessage({
              kind: "drainForUpdate",
              updateId,
              fromTabId: options.tabId,
            } satisfies SwChannelMessage);
          })
        : Promise.resolve();

    await Promise.all([options.onDrainForUpdate().catch(() => {}), peersWait]);

    activeUpdateId = null;
    waiting.postMessage({ kind: "SKIP_WAITING" });
  };

  const status = async (): Promise<SwStatus> => {
    const reg = registration ?? (await readyPromise);
    const controller = swContainer.controller;
    const buildId = controller ? await queryWorkerBuildId(controller) : null;
    return {
      supported: true,
      registered: Boolean(reg),
      controlled: Boolean(controller),
      waiting: Boolean(reg?.waiting),
      buildId,
    };
  };

  return {
    applyUpdate,
    checkForUpdate,
    status,
    dispose: () => {
      disposed = true;
      swContainer.removeEventListener("controllerchange", onControllerChange);
      channel?.close();
    },
  };
}
