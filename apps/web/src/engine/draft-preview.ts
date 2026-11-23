import {
  type CollabEvent,
  Engine,
  type EngineCommand,
  type EngineEvent,
  encodeWalInit,
  type SyncCommand,
} from "@reticula/protocol";
import type { WorkerOutput } from "./client.ts";

export interface DraftPreviewHost {
  post(msg: WorkerOutput, transfer?: Transferable[]): void;
  getLastSubscription(): Extract<EngineCommand, { kind: "subscribe" }> | null;
  setActiveSheet(sheet: number): void;
  getActiveSheet(): number;
}

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
    this.#resubscribe(outcome.sheet.sheet);
    this.#flushPagesAndAux();
  }

  applyReplayTxn(txn: ArrayBuffer): void {
    if (this.#closed) return;
    const sheet = this.#engine.applyWireTxnV3(new Uint8Array(txn));
    this.#host.setActiveSheet(sheet.sheet);
    this.#host.post({ kind: "draftReady", sheet });
    this.#flushPagesAndAux();
  }

  handleCommand(command: EngineCommand): void {
    if (this.#closed) return;
    if (
      command.kind === "subscribe" ||
      command.kind === "switchSheet" ||
      command.kind === "inspectCell" ||
      command.kind === "setEvalEnv"
    ) {
      const ev = this.#engine.handle(command);
      if (ev) this.#emitEngineEvent(ev);
      this.#flushPagesAndAux();
      return;
    }
    this.#host.post({
      kind: "rejected",
      reason: "Exit AI draft preview before editing.",
    });
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
        this.#flushPagesAndAux();
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
        this.#flushPagesAndAux();
      }
    }
  }

  #emitEngineEvent(ev: EngineEvent): void {
    if (ev.kind === "ready" || ev.kind === "sheetChanged") {
      this.#host.setActiveSheet(ev.sheet.sheet);
      this.#host.post({ kind: "draftReady", sheet: ev.sheet });
      this.#resubscribe(ev.sheet.sheet);
      return;
    }
    if (ev.kind === "recalcDone") {
      this.#host.setActiveSheet(ev.sheet.sheet);
      this.#host.post(ev);
      return;
    }
    this.#host.post(ev);
  }

  #resubscribe(sheet: number): void {
    const sub = this.#host.getLastSubscription();
    this.#engine.handle({
      kind: "subscribe",
      sheet,
      epoch: sub?.epoch ?? 1,
      firstRow: sub?.firstRow ?? 0,
      rowCount: sub?.rowCount ?? 64,
      firstCol: sub?.firstCol ?? 0,
      colCount: sub?.colCount ?? 16,
    });
  }

  #flushPagesAndAux(): void {
    if (this.#closed) return;
    while (this.#engine.hasPendingRecalc()) {
      const ev = this.#engine.handle({ kind: "continueRecalc" });
      if (ev) this.#emitEngineEvent(ev);
    }
    for (let page = this.#engine.takePage(); page; page = this.#engine.takePage()) {
      const buf = page.buffer as ArrayBuffer;
      this.#host.post(buf, [buf]);
    }
    for (let aux = this.#engine.takeAuxEvent(); aux; aux = this.#engine.takeAuxEvent()) {
      this.#host.post(aux);
    }
  }
}
