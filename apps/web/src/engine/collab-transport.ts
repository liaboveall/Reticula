import type { CollabEvent } from "@reticula/protocol";
import { report, syncUrl } from "@reticula/protocol/collab-api";
import type { EngineClient } from "./client.ts";

export interface CollabTransportNotice {
  readonly replaced?: boolean;
  readonly upgradeRequired?: boolean;
  readonly authError?: string;
  readonly notice?: string;
}

export interface CollabTransportOptions {
  readonly docId: string;
  readonly shareToken?: string | null | undefined;
  readonly client: EngineClient;
  readonly kernelVersion: () => string;
  readonly onConnectionEvent?: (info: CollabTransportNotice) => void;
  readonly onCheckSwUpdate?: () => void;
}

export interface CollabTransportHandle {
  handleEvent(event: CollabEvent): Promise<void>;
  setOffline(offline: boolean): void;
  dispose(): void;
}

const INITIAL_BACKOFF_MS = 250;
const MAX_BACKOFF_MS = 5_000;
const EXPIRE_PRESENCE_MS = 15_000;
const IDLE_CHECK_MS = 15_000;
const IDLE_CLOSE_MS = 58_000;

export function createCollabTransport(options: CollabTransportOptions): CollabTransportHandle {
  const { docId, shareToken, client, kernelVersion, onConnectionEvent, onCheckSwUpdate } = options;

  let ws: WebSocket | null = null;
  let disposed = false;
  let stoppedPermanently = false;
  let manualOffline = false;
  let reconnectTimer = 0;
  let expireTimer = 0;
  let idleTimer = 0;
  let backoffMs = INITIAL_BACKOFF_MS;
  let lastRxAt = 0;

  const clearTimers = () => {
    if (reconnectTimer !== 0) {
      window.clearTimeout(reconnectTimer);
      reconnectTimer = 0;
    }
    if (expireTimer !== 0) {
      window.clearInterval(expireTimer);
      expireTimer = 0;
    }
    if (idleTimer !== 0) {
      window.clearInterval(idleTimer);
      idleTimer = 0;
    }
  };

  const isBrowserOffline = () =>
    manualOffline || (typeof navigator !== "undefined" && navigator.onLine === false);

  const scheduleReconnect = () => {
    if (disposed || stoppedPermanently || isBrowserOffline() || reconnectTimer !== 0) {
      return;
    }
    const delay = backoffMs;
    backoffMs = Math.min(MAX_BACKOFF_MS, Math.round(backoffMs * 1.8));
    reconnectTimer = window.setTimeout(() => {
      reconnectTimer = 0;
      connect();
    }, delay);
  };

  const closeActiveSocket = () => {
    if (expireTimer !== 0) {
      window.clearInterval(expireTimer);
      expireTimer = 0;
    }
    if (idleTimer !== 0) {
      window.clearInterval(idleTimer);
      idleTimer = 0;
    }
    const current = ws;
    ws = null;
    if (current) {
      current.onopen = null;
      current.onmessage = null;
      current.onerror = null;
      current.onclose = null;
      if (current.readyState === WebSocket.OPEN || current.readyState === WebSocket.CONNECTING) {
        current.close(1000, "client closing");
      }
    }
  };

  const connect = () => {
    if (disposed || stoppedPermanently || isBrowserOffline()) {
      return;
    }
    if (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING)) {
      return;
    }
    const wsProtocol = window.location.protocol === "https:" ? "wss:" : "ws:";
    const url = `${wsProtocol}//${window.location.host}${syncUrl(docId, shareToken ?? undefined)}`;
    const socket = new WebSocket(url);
    socket.binaryType = "arraybuffer";
    ws = socket;

    socket.onopen = () => {
      if (disposed || ws !== socket || isBrowserOffline()) {
        socket.close(1000);
        return;
      }
      backoffMs = INITIAL_BACKOFF_MS;
      lastRxAt = Date.now();
      expireTimer = window.setInterval(() => {
        client.sendSync({ kind: "expirePresence" });
      }, EXPIRE_PRESENCE_MS);
      idleTimer = window.setInterval(() => {
        if (Date.now() - lastRxAt > IDLE_CLOSE_MS) {
          socket.close();
        }
      }, IDLE_CHECK_MS);
      client.sendSync({ kind: "connected" });
    };

    socket.onmessage = (msg) => {
      if (disposed || ws !== socket) {
        return;
      }
      lastRxAt = Date.now();
      if (msg.data instanceof ArrayBuffer) {
        const frame = new Uint8Array(msg.data);
        client.sendSync({ kind: "receive", frame }, [frame.buffer]);
      }
    };

    socket.onerror = () => {
      // `onclose` handles transition to Offline and backoff reconnect.
    };

    socket.onclose = (ev) => {
      if (ws === socket) {
        ws = null;
      }
      if (expireTimer !== 0) {
        window.clearInterval(expireTimer);
        expireTimer = 0;
      }
      if (idleTimer !== 0) {
        window.clearInterval(idleTimer);
        idleTimer = 0;
      }
      if (disposed) {
        return;
      }
      client.sendSync({ kind: "disconnected" });
      if (ev.code === 4001) {
        stoppedPermanently = true;
        onConnectionEvent?.({
          replaced: true,
          notice: "This replica session was replaced by a newer connection.",
        });
        return;
      }
      if (ev.code === 4003) {
        stoppedPermanently = true;
        onConnectionEvent?.({
          authError: "Access to this workbook was revoked.",
        });
        return;
      }
      if (ev.code === 4008) {
        onConnectionEvent?.({
          notice: "Server memory budget is currently busy (4008). Retrying shortly…",
        });
      }
      scheduleReconnect();
    };
  };

  const onOffline = () => {
    if (reconnectTimer !== 0) {
      window.clearTimeout(reconnectTimer);
      reconnectTimer = 0;
    }
    const hadSocket = ws !== null;
    closeActiveSocket();
    if (hadSocket) {
      client.sendSync({ kind: "disconnected" });
    }
  };

  const onOnline = () => {
    if (reconnectTimer !== 0) {
      window.clearTimeout(reconnectTimer);
      reconnectTimer = 0;
    }
    backoffMs = INITIAL_BACKOFF_MS;
    connect();
  };

  window.addEventListener("offline", onOffline);
  window.addEventListener("online", onOnline);
  connect();

  return {
    async handleEvent(event) {
      if (disposed) {
        return;
      }
      switch (event.kind) {
        case "send": {
          if (ws?.readyState === WebSocket.OPEN) {
            ws.send(new Uint8Array(event.bytes));
          }
          break;
        }
        case "fetchSnapshot": {
          try {
            const fetchInit: RequestInit = shareToken
              ? { credentials: "same-origin", headers: { "X-Reticula-Share": shareToken } }
              : { credentials: "same-origin" };
            const resp = await fetch(event.url, fetchInit);
            if (resp.status === 401 || resp.status === 403) {
              stoppedPermanently = true;
              closeActiveSocket();
              onConnectionEvent?.({
                authError: "Access denied while downloading collaborative snapshot.",
              });
              return;
            }
            if (!resp.ok) {
              throw new Error(`Snapshot HTTP ${resp.status}`);
            }
            const buf = await resp.arrayBuffer();
            if (!disposed) {
              client.sendSync(
                { kind: "installSnapshot", seq: event.seq, snapshot: new Uint8Array(buf) },
                [buf],
              );
            }
          } catch {
            closeActiveSocket();
            client.sendSync({ kind: "disconnected" });
            scheduleReconnect();
          }
          break;
        }
        case "reconnect": {
          closeActiveSocket();
          client.sendSync({ kind: "disconnected" });
          backoffMs = INITIAL_BACKOFF_MS;
          if (!isBrowserOffline() && !stoppedPermanently) {
            reconnectTimer = window.setTimeout(() => {
              reconnectTimer = 0;
              connect();
            }, 50);
          }
          break;
        }
        case "upgradeRequired": {
          stoppedPermanently = true;
          closeActiveSocket();
          onCheckSwUpdate?.();
          onConnectionEvent?.({
            upgradeRequired: true,
            notice: "A newer version of Reticula is required by the server. Please reload.",
          });
          break;
        }
        case "epochChanged": {
          onConnectionEvent?.({
            notice: `Server backup restored (epoch ${event.oldEpoch} → ${event.newEpoch}). Local edits are being reconciled.`,
          });
          break;
        }
        case "digestMismatch": {
          void report({
            version: kernelVersion(),
            summary: `StateDigest mismatch doc=${docId} seq=${event.seq} expected=${event.expected} actual=${event.actual}`,
          }).catch(() => {});
          onConnectionEvent?.({
            notice: "State verification mismatch detected; resyncing workbook from server.",
          });
          break;
        }
        case "failed": {
          stoppedPermanently = true;
          closeActiveSocket();
          onConnectionEvent?.({ notice: event.message });
          break;
        }
      }
    },
    setOffline(offline) {
      manualOffline = offline;
      if (offline) {
        onOffline();
      } else {
        onOnline();
      }
    },
    dispose() {
      disposed = true;
      window.removeEventListener("offline", onOffline);
      window.removeEventListener("online", onOnline);
      clearTimers();
      closeActiveSocket();
    },
  };
}
