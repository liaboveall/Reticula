import { Grid, type Selection, type Subscription } from "@reticula/grid";
import {
  type AutoSimProgress,
  AutoSimRunner,
  InteractiveSimLab,
  init,
  type SimLabReplicaView,
  type SimLabState,
} from "@reticula/protocol/sim";
import simWasmUrl from "@reticula/protocol/sim-wasm?url";
import { type FormEvent, useCallback, useEffect, useRef, useState } from "react";
import { plainSelectionText } from "../editor/cell-announcer.ts";
import { translateEngineText } from "../i18n/engine-text.ts";
import { t } from "../i18n/index.ts";
import { tx } from "../i18n/react.ts";
import { errorText } from "../pages/errors.ts";
import { roleName } from "../pages/labels.ts";
import { PageNav } from "../pages/PageNav.tsx";
import { rendererPreference } from "../renderer.ts";
import { ActionButton } from "../ui/ActionButton.tsx";
import { announce } from "../ui/announcer.ts";
import { useKeptFocus } from "../ui/focus.ts";
import { Tabs } from "../ui/Tabs.tsx";
import {
  connectionName,
  type InvariantStatus,
  invariantStatusName,
  invariants,
  parseSeed,
} from "./labels.ts";
import "../collab/collab.css";
import "../pages/pages.css";
import "./sim.css";

export interface SimLabPageProps {
  readonly onNavigate: (path: string) => void;
}

type SimMode = "interactive" | "autorun";

interface EditDraft {
  row: string;
  col: string;
  value: string;
}

const DEFAULT_SUBSCRIPTION: Subscription = {
  epoch: 1,
  firstRow: 0,
  rowCount: 64,
  firstCol: 0,
  colCount: 16,
  visible: { firstRow: 0, endRow: 64, firstCol: 0, endCol: 16 },
  resend: false,
};

const INVARIANT_BADGES: Readonly<Record<InvariantStatus, string>> = {
  idle: "sim-badge",
  checking: "sim-badge",
  verified: "sim-badge sim-badge-ok",
  failed: "sim-badge sim-badge-warn",
};

let initPromise: Promise<void> | null = null;
function ensureSimWasm(): Promise<void> {
  if (!initPromise) {
    initPromise = init({ module_or_path: simWasmUrl }).then(() => undefined);
    // The next visit to the page tries again.
    initPromise.catch(() => {
      initPromise = null;
    });
  }
  return initPromise;
}

function pushPagesForReplica(
  lab: InteractiveSimLab,
  replicaIndex: number,
  grid: Grid,
  sub: Subscription,
): void {
  const firstPr = Math.floor(sub.firstRow / 64);
  const lastPr = Math.floor(Math.max(0, sub.firstRow + sub.rowCount - 1) / 64);
  const firstPc = Math.floor(sub.firstCol / 16);
  const lastPc = Math.floor(Math.max(0, sub.firstCol + sub.colCount - 1) / 16);
  for (let pr = firstPr; pr <= lastPr; pr += 1) {
    for (let pc = firstPc; pc <= lastPc; pc += 1) {
      const bytes = lab.encodePage(replicaIndex, pr, pc, sub.epoch);
      if (bytes) {
        grid.acceptPage(bytes.slice().buffer);
      }
    }
  }
}

/**
 * The grids here are without an editor: their own element has the keyboard.
 * Reads where the selection is, unless that is what was read last.
 */
function announceSelection(host: HTMLElement, selection: Selection, last: string): string {
  const text = plainSelectionText(selection);
  if (host !== document.activeElement || text === last) {
    return last;
  }
  announce(text, { isPlace: true });
  return text;
}

export default function SimLabPage({ onNavigate }: SimLabPageProps) {
  const [wasmReady, setWasmReady] = useState(false);
  const [mode, setMode] = useState<SimMode>("interactive");
  const [replicaCount, setReplicaCount] = useState<2 | 3>(2);
  const [labState, setLabState] = useState<SimLabState | null>(null);
  const [drafts, setDrafts] = useState<Record<number, EditDraft>>({});
  const [labError, setLabError] = useState<string | null>(null);

  const [seedInput, setSeedInput] = useState("42");
  const [autoRunning, setAutoRunning] = useState(false);
  const [autoProgress, setAutoProgress] = useState<AutoSimProgress | null>(null);
  const [autoError, setAutoError] = useState<string | null>(null);

  const mainRef = useKeptFocus();
  const labRef = useRef<InteractiveSimLab | null>(null);
  const hostRefs = useRef<(HTMLDivElement | null)[]>([]);
  const gridRefs = useRef<(Grid | null)[]>([]);
  const subRefs = useRef<Subscription[]>([]);
  const lastReadRef = useRef("");
  const runnerRef = useRef<AutoSimRunner | null>(null);
  const rafRef = useRef<number>(0);

  const syncAllReplicas = useCallback((lab: InteractiveSimLab, initializeSheets: boolean) => {
    const count = lab.replicaCount();
    for (let i = 0; i < count; i += 1) {
      const grid = gridRefs.current[i];
      if (!grid) {
        continue;
      }
      const cfg = lab.sheetInfo(i);
      if (initializeSheets) {
        // The cell the replica's own edits start out with.
        const cell = { row: 0, col: i };
        grid.setSheet(cfg, { selection: { anchor: cell, focus: cell, scope: "cells" } });
      } else {
        grid.updateSheet(cfg);
      }
      const sub = subRefs.current[i] ?? DEFAULT_SUBSCRIPTION;
      pushPagesForReplica(lab, i, grid, sub);
    }
  }, []);

  const resetLab = useCallback(
    (count: 2 | 3) => {
      setLabError(null);
      for (let i = 0; i < gridRefs.current.length; i += 1) {
        gridRefs.current[i]?.destroy();
        gridRefs.current[i] = null;
      }
      labRef.current?.free();
      const nextLab = new InteractiveSimLab(count);
      labRef.current = nextLab;
      setLabState(nextLab.inspectState());
      const nextDrafts: Record<number, EditDraft> = {};
      for (let i = 0; i < count; i += 1) {
        nextDrafts[i] = { row: "0", col: String(i), value: String((i + 1) * 10) };
        const host = hostRefs.current[i];
        if (!host) {
          continue;
        }
        const sub = { ...DEFAULT_SUBSCRIPTION };
        subRefs.current[i] = sub;
        const grid = new Grid(host, {
          subscribe: (nextSub) => {
            subRefs.current[i] = nextSub;
            const curLab = labRef.current;
            if (curLab && i < curLab.replicaCount()) {
              pushPagesForReplica(curLab, i, grid, nextSub);
            }
          },
          onSelectionChange: (sel) => {
            if (!sel) {
              return;
            }
            lastReadRef.current = announceSelection(host, sel, lastReadRef.current);
            setDrafts((prev) => ({
              ...prev,
              [i]: {
                row: String(sel.row),
                col: String(sel.col),
                value: prev[i]?.value ?? "",
              },
            }));
          },
        });
        gridRefs.current[i] = grid;
        grid.setRenderer(rendererPreference());
      }
      setDrafts(nextDrafts);
      syncAllReplicas(nextLab, true);
    },
    [syncAllReplicas],
  );

  useEffect(() => {
    let active = true;
    void ensureSimWasm()
      .then(() => {
        if (active) {
          setWasmReady(true);
        }
      })
      .catch((err) => {
        if (active) {
          setLabError(errorText(err, t("Could not load simulator WASM.")));
        }
      });
    return () => {
      active = false;
      window.cancelAnimationFrame(rafRef.current);
      for (let i = 0; i < gridRefs.current.length; i += 1) {
        gridRefs.current[i]?.destroy();
        gridRefs.current[i] = null;
      }
      labRef.current?.free();
      labRef.current = null;
      runnerRef.current?.free();
      runnerRef.current = null;
    };
  }, []);

  useEffect(() => {
    if (!wasmReady || mode !== "interactive") {
      return;
    }
    resetLab(replicaCount);
    return () => {
      for (let i = 0; i < gridRefs.current.length; i += 1) {
        gridRefs.current[i]?.destroy();
        gridRefs.current[i] = null;
      }
      labRef.current?.free();
      labRef.current = null;
    };
  }, [wasmReady, mode, replicaCount, resetLab]);

  const replicaName = (idx: number) =>
    labState?.replicas[idx]?.name ?? t("Replica {n}", { n: idx + 1 });
  const gridNames = [0, 1, 2].map((idx) => t("Worksheet of {name}", { name: replicaName(idx) }));
  const gridTexts = useRef({ names: gridNames, selectedCell: "" });
  gridTexts.current = { names: gridNames, selectedCell: t("Selected cell") };
  const allGridTexts = JSON.stringify(gridTexts.current);
  // biome-ignore lint/correctness/useExhaustiveDependencies: the grids are other ones with every lab
  useEffect(() => {
    const { names, selectedCell } = gridTexts.current;
    for (const [idx, grid] of gridRefs.current.entries()) {
      grid?.setStrings({ grid: names[idx] ?? "", cellEditor: "", selectedCell });
    }
  }, [allGridTexts, labState]);

  const runInteractiveOp = (fn: (lab: InteractiveSimLab) => SimLabState) => {
    const lab = labRef.current;
    if (!lab) {
      return;
    }
    setLabError(null);
    try {
      const nextState = fn(lab);
      setLabState(nextState);
      syncAllReplicas(lab, false);
    } catch (err) {
      setLabError(err instanceof Error ? err.message : String(err));
    }
  };

  const handleEditSubmit = (event: FormEvent, idx: number) => {
    event.preventDefault();
    const draft = drafts[idx] ?? { row: "0", col: "0", value: "" };
    const row = Math.max(0, Number.parseInt(draft.row, 10) || 0);
    const col = Math.max(0, Number.parseInt(draft.col, 10) || 0);
    runInteractiveOp((lab) => lab.editCell(idx, row, col, draft.value));
  };

  const stopAutoRunner = useCallback(() => {
    window.cancelAnimationFrame(rafRef.current);
    rafRef.current = 0;
    runnerRef.current?.free();
    runnerRef.current = null;
    setAutoRunning(false);
  }, []);

  const handleStartAutoRun = () => {
    if (!wasmReady) {
      return;
    }
    stopAutoRunner();
    setAutoError(null);
    const seed = parseSeed(seedInput.trim() || "42");
    if (seed === null) {
      setAutoError(t("Seed must be an unsigned 64-bit integer."));
      return;
    }
    const runner = new AutoSimRunner(seed);
    runnerRef.current = runner;
    setAutoRunning(true);

    const tick = () => {
      const cur = runnerRef.current;
      if (!cur) {
        return;
      }
      try {
        const progress = cur.stepBatch(15);
        setAutoProgress(progress);
        if (progress.finished) {
          setAutoRunning(false);
          if (progress.error) {
            setAutoError(progress.error);
          }
          return;
        }
        rafRef.current = window.requestAnimationFrame(tick);
      } catch (err) {
        setAutoRunning(false);
        setAutoError(err instanceof Error ? err.message : String(err));
      }
    };
    rafRef.current = window.requestAnimationFrame(tick);
  };

  const progressPct =
    autoProgress && autoProgress.totalSteps > 0
      ? Math.min(100, Math.round((autoProgress.completedSteps / autoProgress.totalSteps) * 100))
      : 0;

  const invariantStatus: InvariantStatus = !autoProgress
    ? "idle"
    : !autoProgress.finished
      ? "checking"
      : autoProgress.passed
        ? "verified"
        : "failed";

  return (
    <div className="page-shell sim-shell" data-testid="sim-lab-page">
      <PageNav
        onNavigate={onNavigate}
        links={[
          ["/app", t("Open Editor")],
          ["/workbooks", t("Workbooks")],
        ]}
      />

      <main ref={mainRef} className="sim-main">
        <Tabs
          label={t("Sync & CRDT Simulator Lab")}
          tabs={[
            {
              key: "interactive",
              text: t("Interactive Replicas"),
              testId: "sim-mode-interactive",
            },
            { key: "autorun", text: t("5-Replica Auto-Run"), testId: "sim-mode-autorun" },
          ]}
          selected={mode}
          onSelect={(next) => {
            if (next === "interactive") {
              stopAutoRunner();
            }
            setMode(next);
          }}
          listClassName="page-tabs"
          tabClassName="page-tab"
          header={(list) => (
            <header className="page-header">
              <div>
                <h1>{t("Sync & CRDT Simulator Lab")}</h1>
                <p className="page-muted">
                  {tx(
                    "Dedicated <0>reticula-sim-wasm</0> engine loaded on demand: step offline replica merges interactively or run the 5-replica fault injector inside your browser.",
                    { 0: <code /> },
                  )}
                </p>
              </div>
              {list}
            </header>
          )}
        >
          {!wasmReady ? (
            <div className="page-card">
              {labError ? (
                <div className="page-error" role="alert" data-testid="sim-lab-error">
                  {translateEngineText(labError)}
                </div>
              ) : (
                <p role="status">{t("Loading simulator WebAssembly module…")}</p>
              )}
            </div>
          ) : mode === "interactive" ? (
            <>
              {labError && (
                <div className="page-error" role="alert" data-testid="sim-lab-error">
                  {translateEngineText(labError)}
                </div>
              )}

              <div className="sim-controls-bar">
                <div className="collab-row">
                  <label htmlFor="sim-replica-count">{t("Replicas:")}</label>
                  <select
                    id="sim-replica-count"
                    className="collab-select"
                    data-testid="sim-replica-count"
                    value={replicaCount}
                    onChange={(e) => setReplicaCount(e.target.value === "3" ? 3 : 2)}
                  >
                    <option value="2">{t("2 Replicas (Alice & Bob)")}</option>
                    <option value="3">{t("3 Replicas (Alice, Bob & Carol)")}</option>
                  </select>
                  <button
                    type="button"
                    className="toolbar-btn"
                    data-testid="sim-reset-btn"
                    onClick={() => resetLab(replicaCount)}
                  >
                    {t("Reset Lab")}
                  </button>
                </div>
                <div className="collab-row">
                  <ActionButton
                    className="toolbar-btn"
                    data-testid="sim-deliver-one"
                    inactive={!labState || labState.queue.length === 0}
                    onClick={() => runInteractiveOp((lab) => lab.deliverNext())}
                  >
                    {t("Deliver 1")}
                  </ActionButton>
                  <ActionButton
                    className="toolbar-btn primary-btn"
                    data-testid="sim-deliver-all"
                    inactive={!labState || labState.queue.length === 0}
                    onClick={() => runInteractiveOp((lab) => lab.deliverAll())}
                  >
                    {t("Deliver All ({n})", { n: labState?.queue.length ?? 0 })}
                  </ActionButton>
                </div>
              </div>

              {labState && (
                <div className="sim-server-banner">
                  <div className="collab-row">
                    <strong>{t("Server State")}</strong>
                    <span>{t("headSeq #{seq}", { seq: labState.serverHeadSeq })}</span>
                    <span>{t("epoch {epoch}", { epoch: labState.serverEpoch })}</span>
                    <span className="sim-digest-code" title={labState.serverDigest}>
                      {t("digest {digest}…", { digest: labState.serverDigest.slice(0, 16) })}
                    </span>
                  </div>
                  <span
                    role="status"
                    data-testid="sim-convergence-badge"
                    className={
                      labState.allConverged ? "sim-badge sim-badge-ok" : "sim-badge sim-badge-warn"
                    }
                  >
                    {labState.allConverged
                      ? t("All Replicas Converged")
                      : t("Diverged / Pending Sync")}
                  </span>
                </div>
              )}

              <div className="sim-replicas-grid">
                {[0, 1, 2].slice(0, replicaCount).map((idx) => {
                  const rep: SimLabReplicaView | undefined = labState?.replicas[idx];
                  const draft = drafts[idx] ?? { row: "0", col: String(idx), value: "" };
                  return (
                    <section
                      key={`replica-${idx}`}
                      className="sim-replica-card"
                      aria-labelledby={`sim-replica-name-${idx}`}
                      data-testid={`sim-replica-card-${idx}`}
                    >
                      <div className="sim-replica-header">
                        <strong id={`sim-replica-name-${idx}`}>
                          {rep
                            ? t("{name} ({role})", { name: rep.name, role: roleName(rep.role) })
                            : t("Replica {n}", { n: idx + 1 })}
                        </strong>
                        <span
                          data-testid={`sim-replica-status-${idx}`}
                          className={
                            rep?.converged ? "sim-badge sim-badge-ok" : "sim-badge sim-badge-warn"
                          }
                        >
                          {rep?.converged
                            ? t("Converged")
                            : connectionName(rep?.connection ?? "initializing")}
                        </span>
                      </div>
                      {rep && (
                        <div className="sim-replica-meta">
                          <span>{t("seq #{seq}", { seq: rep.confirmedSeq })}</span>
                          <span>{t("pending {n}", { n: rep.pendingLen })}</span>
                          <span>{t("rep {id}", { id: rep.replicaId ?? "—" })}</span>
                          <span className="sim-digest-code" title={rep.digest}>
                            {rep.digest.slice(0, 12)}…
                          </span>
                        </div>
                      )}

                      <div
                        ref={(el) => {
                          hostRefs.current[idx] = el;
                        }}
                        className="sim-grid-host"
                        data-testid={`sim-grid-${idx}`}
                      />

                      <form className="sim-edit-row" onSubmit={(e) => handleEditSubmit(e, idx)}>
                        <input
                          type="number"
                          min={0}
                          className="sim-num-input"
                          data-testid={`sim-row-${idx}`}
                          aria-label={t("Replica {n} row", { n: idx })}
                          value={draft.row}
                          onChange={(e) =>
                            setDrafts((prev) => ({
                              ...prev,
                              [idx]: { ...draft, row: e.target.value },
                            }))
                          }
                        />
                        <input
                          type="number"
                          min={0}
                          className="sim-num-input"
                          data-testid={`sim-col-${idx}`}
                          aria-label={t("Replica {n} column", { n: idx })}
                          value={draft.col}
                          onChange={(e) =>
                            setDrafts((prev) => ({
                              ...prev,
                              [idx]: { ...draft, col: e.target.value },
                            }))
                          }
                        />
                        <input
                          type="text"
                          className="collab-input"
                          data-testid={`sim-value-${idx}`}
                          aria-label={t("Replica {n} cell input", { n: idx })}
                          placeholder={t("Value or =formula")}
                          value={draft.value}
                          onChange={(e) =>
                            setDrafts((prev) => ({
                              ...prev,
                              [idx]: { ...draft, value: e.target.value },
                            }))
                          }
                        />
                        <button
                          type="submit"
                          className="toolbar-btn primary-btn"
                          data-testid={`sim-apply-${idx}`}
                        >
                          {t("Apply")}
                        </button>
                      </form>

                      <div className="sim-edit-row">
                        <button
                          type="button"
                          className="toolbar-btn"
                          data-testid={`sim-insert-row-${idx}`}
                          onClick={() => runInteractiveOp((lab) => lab.insertRow(idx, -1))}
                        >
                          + {t("Row Top")}
                        </button>
                        <ActionButton
                          className="toolbar-btn"
                          data-testid={`sim-undo-${idx}`}
                          inactive={!rep?.canUndo}
                          onClick={() => runInteractiveOp((lab) => lab.undo(idx))}
                        >
                          {t("Undo")}
                        </ActionButton>
                        <select
                          className="collab-select"
                          data-testid={`sim-link-${idx}`}
                          aria-label={t("Replica {n} network link", { n: idx })}
                          value={rep?.linkMode ?? "online"}
                          onChange={(e) => {
                            const val = e.target.value;
                            runInteractiveOp((lab) => lab.setLinkState(idx, val));
                          }}
                        >
                          <option value="online">{t("Link: Online")}</option>
                          <option value="delayed">{t("Link: Delayed")}</option>
                          <option value="dropped">{t("Link: Dropped")}</option>
                        </select>
                        <button
                          type="button"
                          className="toolbar-btn"
                          data-testid={`sim-crash-${idx}`}
                          onClick={() => runInteractiveOp((lab) => lab.crashAndRestart(idx))}
                        >
                          {t("Crash & Restart")}
                        </button>
                      </div>
                      {rep?.lastNotice && (
                        <p className="page-muted">{translateEngineText(rep.lastNotice)}</p>
                      )}
                    </section>
                  );
                })}
              </div>

              <section className="page-card" data-testid="sim-wire-inspector">
                <h2>
                  {t("Wire Queue Inspector ({n} frames queued)", {
                    n: labState?.queue.length ?? 0,
                  })}
                </h2>
                {!labState || labState.queue.length === 0 ? (
                  <p className="page-muted">
                    {t(
                      "No queued network frames. Switch a replica link to Delayed to hold frames for step-by-step inspection.",
                    )}
                  </p>
                ) : (
                  <ul className="sim-queue-list">
                    {(() => {
                      const counts = new Map<string, number>();
                      return labState.queue.map((item) => {
                        const base = `${item.replicaIndex}-${item.direction}-${item.summary}`;
                        const occ = (counts.get(base) ?? 0) + 1;
                        counts.set(base, occ);
                        return (
                          <li key={`${base}-${occ}`} className="sim-queue-item">
                            <span>
                              <strong>{item.replicaName}</strong> ·{" "}
                              <code>{item.direction.toUpperCase()}</code> ·{" "}
                              {translateEngineText(item.summary)}
                            </span>
                          </li>
                        );
                      });
                    })()}
                  </ul>
                )}
              </section>
            </>
          ) : (
            <section className="page-card">
              {autoError && (
                <div className="page-error" role="alert" data-testid="sim-autorun-error">
                  {translateEngineText(autoError)}
                </div>
              )}

              <div className="sim-edit-row" style={{ marginBottom: "12px" }}>
                <label htmlFor="sim-seed-input">{t("Deterministic Seed:")}</label>
                <input
                  id="sim-seed-input"
                  type="text"
                  className="collab-input"
                  style={{ maxWidth: "160px" }}
                  data-testid="sim-seed-input"
                  value={seedInput}
                  disabled={autoRunning}
                  onChange={(e) => setSeedInput(e.target.value)}
                />
                <ActionButton
                  className="toolbar-btn primary-btn"
                  data-testid="sim-autorun-btn"
                  inactive={autoRunning}
                  onClick={handleStartAutoRun}
                >
                  {autoRunning ? t("Running…") : t("Run Seed")}
                </ActionButton>
                <button
                  type="button"
                  className="toolbar-btn"
                  onClick={() => {
                    stopAutoRunner();
                    setAutoProgress(null);
                    setAutoError(null);
                  }}
                >
                  {t("Reset")}
                </button>
              </div>

              <div
                className="sim-progress-track"
                style={{ marginBottom: "10px" }}
                role="progressbar"
                aria-label={t("5-Replica Auto-Run")}
                aria-valuemin={0}
                aria-valuemax={100}
                aria-valuenow={progressPct}
              >
                <div className="sim-progress-fill" style={{ width: `${progressPct}%` }} />
              </div>

              {autoProgress && (
                <>
                  <div className="sim-server-banner">
                    <div className="collab-row">
                      <span>
                        {tx(
                          "Steps: <0>{done} / {total}</0>",
                          { 0: <strong /> },
                          { done: autoProgress.completedSteps, total: autoProgress.totalSteps },
                        )}
                      </span>
                      <span>{t("server headSeq #{seq}", { seq: autoProgress.headSeq })}</span>
                      <span>{t("epoch {epoch}", { epoch: autoProgress.epoch })}</span>
                      <span className="sim-digest-code" title={autoProgress.serverDigest}>
                        {autoProgress.serverDigest}
                      </span>
                    </div>
                    <span
                      role="status"
                      data-testid="sim-autorun-status"
                      className={
                        autoProgress.finished
                          ? autoProgress.passed
                            ? "sim-badge sim-badge-ok"
                            : "sim-badge sim-badge-warn"
                          : "sim-badge"
                      }
                    >
                      {autoProgress.finished
                        ? autoProgress.passed
                          ? t("{n} Invariants Verified", { n: invariants().length })
                          : t("Verification Failed")
                        : t("Running…")}
                    </span>
                  </div>

                  <h2>{t("Fault & Operation Telemetry")}</h2>
                  <div className="sim-fault-grid" style={{ marginBottom: "14px" }}>
                    <div className="sim-fault-cell">
                      <span className="page-muted">{t("Local edits")}</span>
                      <strong>{autoProgress.faults.localEdits}</strong>
                    </div>
                    <div className="sim-fault-cell">
                      <span className="page-muted">{t("SetCellsIf / Formula")}</span>
                      <strong>
                        {autoProgress.faults.setCellsIfOps} / {autoProgress.faults.formulaOps}
                      </strong>
                    </div>
                    <div className="sim-fault-cell">
                      <span className="page-muted">{t("Client crashes / OPFS loss")}</span>
                      <strong>
                        {autoProgress.faults.clientCrashes} / {autoProgress.faults.opfsTailLosses}
                      </strong>
                    </div>
                    <div className="sim-fault-cell">
                      <span className="page-muted">{t("Server crashes / Epoch bumps")}</span>
                      <strong>
                        {autoProgress.faults.serverCrashes} /{" "}
                        {autoProgress.faults.epochBackupRestores}
                      </strong>
                    </div>
                    <div className="sim-fault-cell">
                      <span className="page-muted">{t("Snapshot catchups / Rejects")}</span>
                      <strong>
                        {autoProgress.faults.snapshotRequiredCatchups} /{" "}
                        {autoProgress.faults.terminalRejects}
                      </strong>
                    </div>
                    <div className="sim-fault-cell">
                      <span className="page-muted">{t("Rogue frames rejected")}</span>
                      <strong>{autoProgress.faults.rogueAttemptsRejected}</strong>
                    </div>
                  </div>

                  <h2>{t("5 Simulated Collaborators")}</h2>
                  <div className="sim-table-wrap">
                    <table className="sim-table">
                      <thead>
                        <tr>
                          <th>{t("Replica")}</th>
                          <th>{t("Role")}</th>
                          <th>{t("Replica ID")}</th>
                          <th>{t("Connection")}</th>
                          <th>{t("Confirmed Seq")}</th>
                          <th>{t("Pending")}</th>
                        </tr>
                      </thead>
                      <tbody>
                        {autoProgress.replicas.map((r) => (
                          <tr key={r.index}>
                            <td>#{r.index}</td>
                            <td>{roleName(r.role)}</td>
                            <td>{r.replica ?? "—"}</td>
                            <td>{connectionName(r.connection)}</td>
                            <td>#{r.confirmedSeq}</td>
                            <td>{r.pendingLen}</td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                </>
              )}

              <h2>{t("Protocol Invariants (1–{n})", { n: invariants().length })}</h2>
              <ul className="sim-invariants-grid" data-testid="sim-invariants-list">
                {invariants().map((text, index) => (
                  // biome-ignore lint/suspicious/noArrayIndexKey: an invariant is known by its number
                  <li key={index} className="sim-invariant-item">
                    <span>
                      {index + 1}. {text}
                    </span>
                    <span className={INVARIANT_BADGES[invariantStatus]}>
                      {invariantStatusName(invariantStatus)}
                    </span>
                  </li>
                ))}
              </ul>
            </section>
          )}
        </Tabs>
      </main>
    </div>
  );
}
