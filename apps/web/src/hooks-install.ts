import type { EditorHandle } from "./commands/handle.ts";
import { loadLater } from "./editor/load-later.ts";
import type { EngineMessage } from "./engine/client.ts";
import type { CollabTransportHandle } from "./engine/collab-transport.ts";
import type { CollabHookStatus, HookDocStatus, Hooks, TableHooks } from "./hooks.ts";
import { setRendererPreference } from "./renderer.ts";
import type { SwClientHandle } from "./sw-client.ts";
import { readCatalog } from "./tables/catalog.ts";
import type { TableClient } from "./tables/client.ts";
import { deleteTables, tablesLock } from "./tables/delete.ts";
import { lastTables } from "./tables/shell.ts";
import type { TableModel } from "./tables/ui/model.ts";

/** A part of the editor's state that is not rendered. */
interface Ref<T> {
  readonly current: T;
}

/** What the hooks report on and act upon besides what the commands do. */
export interface HookHost {
  readonly internals: Ref<{
    readonly flushAndClose: () => Promise<void>;
    readonly swClient: () => Promise<SwClientHandle>;
  } | null>;
  readonly docId: Ref<string>;
  readonly title: Ref<string>;
  readonly engineStatus: Ref<HookDocStatus["engineStatus"]>;
  readonly storageBackend: Ref<HookDocStatus["storageBackend"]>;
  readonly activeSlot: Ref<HookDocStatus["activeSlot"]>;
  readonly walSeq: Ref<string>;
  readonly waitingForFile: Ref<boolean>;
  readonly safeMode: Ref<HookDocStatus["safeMode"]>;
  readonly connection: Ref<CollabHookStatus["connection"]>;
  readonly replica: Ref<number | null>;
  readonly pending: Ref<number>;
  readonly slot: Ref<string | null>;
  readonly manualOffline: { current: boolean };
  readonly transport: Ref<Pick<CollabTransportHandle, "setOffline"> | null>;
}

/** Resolves to what the engine says about something, once it does. */
function answer<T>(
  handle: EditorHandle,
  read: (message: EngineMessage) => T | undefined,
  ask: () => void,
): Promise<T> {
  return new Promise((resolve) => {
    const stop = handle.intercept((message) => {
      const found = read(message);
      if (found === undefined) {
        return false;
      }
      stop();
      resolve(found);
      return message.kind !== "docSaved";
    });
    ask();
  });
}

/** The data table in view. */
function tableHooks(handle: EditorHandle): TableHooks | null {
  // The only view of a table there is.
  const model = handle.state().table?.view as TableModel | null | undefined;
  const grid = model?.grid();
  return model && grid
    ? {
        stats: () => grid.stats(),
        scrollTo: (position) => grid.scrollTo(position),
        textAt: (row, col) => grid.textAt(row, col),
        ask: (query) => model.ask(query),
        // The only tables that are opened.
        client: () => lastTables() as TableClient | null,
      }
    : null;
}

export function installHooks(handle: EditorHandle, host: HookHost): () => void {
  const grid = handle.grid();
  const { internals } = host;
  if (!grid) {
    return () => {};
  }
  const swClient = () =>
    internals.current?.swClient() ?? Promise.reject(new Error("the editor is gone"));
  const hooks: Hooks = {
    stats: () => grid.stats(),
    scrollTo: (position) => grid.scrollTo(position),
    select: (row, col) => grid.select({ row, col }),
    selectRange: (anchor, focus) => grid.selectRange(anchor, focus),
    textAt: (row, col) => grid.textAt(row, col),
    selfCheck: () =>
      answer(
        handle,
        (message) => (message.kind === "selfChecked" ? message.digest : undefined),
        () => handle.send({ kind: "selfCheck" }),
      ),
    stateDigest: () =>
      answer(
        handle,
        (message) => (message.kind === "stateDigested" ? message.digest : undefined),
        () => handle.send({ kind: "stateDigest" }),
      ),
    docStatus: () => ({
      docId: host.docId.current,
      title: host.title.current,
      engineStatus: host.engineStatus.current,
      storageBackend: host.storageBackend.current,
      activeSlot: host.activeSlot.current,
      walSeq: host.walSeq.current,
      waitingForFile: host.waitingForFile.current,
      safeMode: host.safeMode.current,
    }),
    compactSnapshot: () =>
      answer(
        handle,
        (message) => (message.kind === "docSaved" ? message : undefined),
        () => handle.sendHost({ kind: "forceCompactSnapshot" }),
      ).then(({ docId, slot, byteLen, elapsedMs }) => ({ docId, slot, byteLen, elapsedMs })),
    flushAndClose: () => internals.current?.flushAndClose() ?? Promise.resolve(),
    spinWorker: (ms) => handle.sendHost({ kind: "debugSpinWorker", ms }),
    swStatus: async () => (await swClient()).status(),
    checkSwUpdate: async () => (await swClient()).checkForUpdate(),
    collabStatus: () => {
      const collab = handle.collab();
      return {
        active: collab !== null,
        docId: collab?.docId ?? null,
        connection: host.connection.current,
        replica: host.replica.current,
        pending: host.pending.current,
        slot: host.slot.current,
        remotePresences: handle.presences(),
      };
    },
    setCollabOffline: (offline) => {
      host.manualOffline.current = offline;
      host.transport.current?.setOffline(offline);
    },
    tables: async () => ({
      readCatalog,
      deleteTables,
      tablesLock,
      ...(await (await loadLater()).loadTables()),
    }),
    tableStatus: () => lastTables()?.status() ?? null,
    crashDataWorker: () => lastTables()?.crash(),
    tableMemoryLimit: (bytes) => lastTables()?.setMemoryLimit(bytes) ?? Promise.resolve(),
    tableEntries: () => handle.tables(),
    showTable: (id) => handle.showTable(id),
    table: () => tableHooks(handle),
    setRenderer: (preference) => setRendererPreference(grid, preference),
  };
  window.__reticula = hooks;
  return () => {
    if (window.__reticula === hooks) {
      delete window.__reticula;
    }
  };
}
