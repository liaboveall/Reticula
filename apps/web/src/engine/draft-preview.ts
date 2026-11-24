import {
  type CollabEvent,
  Engine,
  type EngineCommand,
  type EngineEvent,
  encodeWalInit,
  type SyncCommand,
} from "@reticula/protocol";
import type { WorkerOutput } from "./client.ts";
import type { PageSource } from "./page-pump.ts";

export interface DraftPreviewHost {
  post(msg: WorkerOutput, transfer?: Transferable[]): void;
  /** Posts due pages for one slice, and asks for more in later ones. */
  postPages(source: PageSource): void;
  setActiveSheet(sheet: number): void;
  getActiveSheet(): number;
}

/** What a draft can be asked: what reads it, and what shows another part of it. */
const ALLOWED_COMMANDS: ReadonlySet<EngineCommand["kind"]> = new Set<EngineCommand["kind"]>([
  "subscribe",
  "switchSheet",
  "inspectCell",
  "setEvalEnv",
  "jumpEdge",
  "jumpUsedEnd",
  "copyRange",
  "findAll",
  "findNext",
]);

export class DraftPreviewSession {
  #engine: Engine;
  #host: DraftPreviewHost;
  #ws: WebSocket | null = null;
  #closed = false;
  #queue: Promise<void> = Promise.resolve();

  constructor(host: DraftPreviewHost) {
    this.#host = host;
    this.#engine = new Engine();
  }

  openCloud(docId: string, branchId: string): void {
    const ready = this.#engine.handleSync({
      kind: "open",
      docId,
      branchId,
      info: undefined,
      snapshot: new Uint8Array(),
      pendingWal: new Uint8Array(),
    });
    if (ready) this.#emitEngineEvent(ready);
    this.#drainCollab();

    const proto = self.location.protocol === "https:" ? "wss:" : "ws:";
    const url = `${proto}//${self.location.host}/api/sync/${encodeURIComponent(docId)}?branch=${encodeURIComponent(branchId)}`;
    const ws = new WebSocket(url);
    ws.binaryType = "arraybuffer";
    this.#ws = ws;
    ws.onopen = () => {
      if (this.#closed || this.#ws !== ws) return;
      this.#enqueueSync({ kind: "connected" });
    };
    ws.onmessage = (ev: MessageEvent) => {
      if (this.#closed || this.#ws !== ws || !(ev.data instanceof ArrayBuffer)) return;
      this.#enqueueSync({ kind: "receive", frame: new Uint8Array(ev.data) });
    };
    ws.onclose = () => {
      if (this.#closed || this.#ws !== ws) return;
      this.#enqueueSync({ kind: "disconnected" });
    };
  }

  openReplay(baseSnapshot: ArrayBuffer): void {
    const outcome = this.#engine.loadSnapshotAndWal(
      new Uint8Array(baseSnapshot),
      encodeWalInit(0),
      0,
      0,
      0,
      true,
      false,
      false,
    );
    this.#host.setActiveSheet(outcome.sheet.sheet);
    this.#host.post({ kind: "draftReady", sheet: outcome.sheet });
    this.#recalcAndFlush();
  }

  applyReplayTxn(txn: ArrayBuffer): void {
    if (this.#closed) return;
    const sheet = this.#engine.applyWireTxnV3(new Uint8Array(txn));
    this.#host.setActiveSheet(sheet.sheet);
    this.#host.post({ kind: "sheetChanged", sheet, actionMs: 0, recalculated: 0 });
    this.#recalcAndFlush();
  }

  handleCommand(command: EngineCommand): void {
    if (this.#closed) return;
    if (!ALLOWED_COMMANDS.has(command.kind)) {
      this.#host.post({
        kind: "rejected",
        reason: "Exit AI draft preview before editing.",
      });
      return;
    }
    const ev = this.#engine.handle(this.#readOnly(command));
    if (ev) this.#emitEngineEvent(ev);
    this.#recalcAndFlush();
  }

  /** Posts pages that did not fit into an earlier slice. */
  flushPages(): void {
    if (this.#closed) return;
    this.#host.postPages(this.#engine);
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    const ws = this.#ws;
    this.#ws = null;
    if (ws) {
      ws.onopen = null;
      ws.onmessage = null;
      ws.onclose = null;
      try {
        ws.close(1000, "draft preview closed");
      } catch {
        // Ignore already-closed WebSocket.
      }
    }
    this.#engine.free();
  }

  #enqueueSync(cmd: SyncCommand): void {
    this.#queue = this.#queue
      .then(async () => {
        if (this.#closed) return;
        const ev = this.#engine.handleSync(cmd);
        if (ev) this.#emitEngineEvent(ev);
        await this.#drainCollab();
        this.#recalcAndFlush();
      })
      .catch(() => {});
  }

  async #drainCollab(): Promise<void> {
    if (this.#closed) return;
    const events: CollabEvent[] = [];
    for (let ev = this.#engine.takeCollabEvent(); ev; ev = this.#engine.takeCollabEvent()) {
      events.push(ev);
    }
    for (const ev of events) {
      if (this.#closed) return;
      if (ev.kind === "send") {
        if (this.#ws?.readyState === WebSocket.OPEN) {
          this.#ws.send(new Uint8Array(ev.bytes));
        }
      } else if (ev.kind === "fetchSnapshot") {
        const resp = await fetch(ev.url, { credentials: "same-origin" });
        if (!resp.ok || this.#closed) continue;
        const snap = new Uint8Array(await resp.arrayBuffer());
        if (this.#closed) return;
        const ready = this.#engine.handleSync({
          kind: "installSnapshot",
          seq: ev.seq,
          snapshot: snap,
        });
        if (ready) this.#emitEngineEvent(ready);
        await this.#drainCollab();
        this.#recalcAndFlush();
      }
    }
  }

  #emitEngineEvent(ev: EngineEvent): void {
    if (ev.kind === "ready") {
      this.#host.setActiveSheet(ev.sheet.sheet);
      this.#host.post({ kind: "draftReady", sheet: ev.sheet });
      return;
    }
    if (ev.kind === "sheetChanged" || ev.kind === "recalcDone") {
      this.#host.setActiveSheet(ev.sheet.sheet);
      this.#host.post(ev);
      return;
    }
    this.#host.post(ev);
  }

  #readOnly(command: EngineCommand): EngineCommand {
    if (command.kind === "subscribe") {
      return { ...command, sheet: command.sheet ?? this.#host.getActiveSheet() };
    }
    // Nothing is taken out of a draft by copying from it.
    return command.kind === "copyRange" ? { ...command, cut: false } : command;
  }

  #recalcAndFlush(): void {
    if (this.#closed) return;
    while (this.#engine.hasPendingRecalc()) {
      const ev = this.#engine.handle({ kind: "continueRecalc" });
      if (ev) this.#emitEngineEvent(ev);
    }
    this.flushPages();
  }
}
