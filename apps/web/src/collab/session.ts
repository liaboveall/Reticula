import type { CellEditor, Grid, RemotePresenceOverlay, Selection } from "@reticula/grid";
import type { CollabEvent } from "@reticula/protocol";
import type { EngineClient } from "../engine/client.ts";
import { type CollabTransportHandle, createCollabTransport } from "../engine/collab-transport.ts";
import type { CollabHookStatus } from "../hooks.ts";
import type { CollabWorkbookTarget } from "./CollabShell.tsx";
import { withPersonTexts } from "./labels.ts";

interface Ref<T> {
  current: T;
}

type Connection = CollabHookStatus["connection"];

/** What of the editor a session with the server has to do with. */
export interface CollabSessionHost {
  readonly target: CollabWorkbookTarget;
  readonly client: EngineClient;
  readonly grid: Grid;
  readonly editor: CellEditor;
  readonly activeSheet: Ref<number>;
  readonly selection: Ref<Selection | null>;
  readonly kernelVersion: Ref<string>;
  readonly connection: Ref<Connection>;
  readonly replica: Ref<number | null>;
  readonly pending: Ref<number>;
  readonly presences: Ref<readonly RemotePresenceOverlay[]>;
  readonly transport: Ref<CollabTransportHandle | null>;
  readonly manualOffline: Ref<boolean>;
  readonly onConnection: (connection: Connection) => void;
  readonly onPending: (pending: number) => void;
  readonly onPresences: (presences: readonly RemotePresenceOverlay[]) => void;
  readonly onNotice: (notice: string) => void;
  readonly onCheckUpdate: () => void;
  readonly sendPresence: () => void;
}

/**
 * Someone else inserted, deleted or moved rows or columns under the cursor:
 * keep it on the same cells, as long as the user has not moved it since and is
 * not typing into a cell.
 */
function followSelection(
  { activeSheet, editor, grid, selection }: CollabSessionHost,
  { sheet, from, to }: Extract<CollabEvent, { kind: "localSelectionMoved" }>,
): void {
  const sel = selection.current;
  if (
    sheet !== activeSheet.current ||
    editor.editing ||
    !sel ||
    sel.scope === "all" ||
    sel.ranges.length !== 1 ||
    sel.row !== from.row ||
    sel.col !== from.col
  ) {
    return;
  }
  const was = from.selection;
  const now = to.selection;
  if (!was || !now) {
    if (!was && !now) {
      grid.select({ row: to.row, col: to.col });
    }
    return;
  }
  if (
    sel.range.firstRow === was.firstRow &&
    sel.range.endRow === was.endRow &&
    sel.range.firstCol === was.firstCol &&
    sel.range.endCol === was.endCol
  ) {
    grid.selectRange(
      { row: to.row, col: to.col },
      {
        row: from.row === was.firstRow ? now.endRow - 1 : now.firstRow,
        col: from.col === was.firstCol ? now.endCol - 1 : now.firstCol,
      },
      sel.scope,
    );
  }
}

/** Connects a workbook to the server. Returns what takes the engine's events. */
export function startCollabSession(host: CollabSessionHost): (event: CollabEvent) => void {
  const { target, client } = host;
  const transport = createCollabTransport({
    docId: target.docId,
    shareToken: target.shareToken,
    client,
    kernelVersion: () => host.kernelVersion.current,
    onCheckSwUpdate: host.onCheckUpdate,
    onConnectionEvent: ({ notice, authError }) => {
      const text = notice || authError;
      if (text) {
        host.onNotice(text);
      }
    },
  });
  if (host.manualOffline.current) {
    transport.setOffline(true);
  }
  host.transport.current = transport;
  let queue = Promise.resolve();

  return (event) => {
    switch (event.kind) {
      case "state": {
        const before = host.connection.current;
        host.connection.current = event.connection;
        host.replica.current = event.replica ?? null;
        host.pending.current = event.pending;
        host.onConnection(event.connection);
        host.onPending(event.pending);
        if (event.connection === "online" && before !== "online") {
          host.sendPresence();
        }
        break;
      }
      case "presence":
        if (event.sheet === host.activeSheet.current) {
          host.presences.current = event.overlays;
          host.onPresences(event.overlays);
          host.grid.setRemotePresences(withPersonTexts(event.overlays));
        }
        break;
      case "localSelectionMoved":
        followSelection(host, event);
        break;
      default:
        if (host.transport.current === transport) {
          queue = queue.then(() => transport.handleEvent(event));
        }
    }
  };
}
