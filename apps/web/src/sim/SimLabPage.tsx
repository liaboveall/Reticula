import { Grid, type Subscription } from "@reticula/grid";
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
};

const TEN_INVARIANTS: readonly string[] = [
  "1. Quiescent StateDigest converges across all online writers & viewers",
  "2. Logical content & formula digests match across all converged replicas",
  "3. Re-evaluating from scratch matches incremental & sliced formula engine",
  "4. Server periodic Digest frames never mismatch any synced replica",
  "5. Durable-before-send & OPFS tail-loss recovery preserves watermark monotonicity",
  "6. Crash & restart restores pending WAL without duplicating or losing ops",
  "7. Epoch increment on backup restore re-identifies & preserves unreconciled work",
  "8. Row/Col/Sheet ID ownership & structural move-anchor handoffs remain valid",
  "9. Full epoch log replay and latest snapshot + tail replay agree bit-for-bit",
  "10. Every terminally rejected client txn is rescued into unsynced local archive",
];

let initPromise: Promise<void> | null = null;
function ensureSimWasm(): Promise<void> {
  if (!initPromise) {
    initPromise = init({ module_or_path: simWasmUrl }).then(() => undefined);
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

  const labRef = useRef<InteractiveSimLab | null>(null);
  const hostRefs = useRef<(HTMLDivElement | null)[]>([]);
  const gridRefs = useRef<(Grid | null)[]>([]);
  const subRefs = useRef<Subscription[]>([]);
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
        grid.setSheet(cfg);
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
          setLabError(err instanceof Error ? err.message : "Could not load simulator WASM.");
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
    let parsedSeed: bigint;
    try {
      parsedSeed = BigInt(seedInput.trim() || "42");
      if (parsedSeed < 0n) {
        throw new Error("Seed must be an unsigned 64-bit integer.");
      }
    } catch (err) {
      setAutoError(err instanceof Error ? err.message : "Invalid unsigned integer seed.");
      return;
    }
    const runner = new AutoSimRunner(parsedSeed);
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

  return (
    <div className="page-shell" data-testid="sim-lab-page">
      <nav className="page-nav">
        <a
          href="/"
          className="page-brand"
          onClick={(e) => {
            e.preventDefault();
            onNavigate("/");
          }}
        >
          Reticula
        </a>
        <div className="page-nav-links">
          <a
            href="/app"
            className="toolbar-btn"
            onClick={(e) => {
              e.preventDefault();
              onNavigate("/app");
            }}
          >
            Open Editor
          </a>
          <a
            href="/workbooks"
            className="toolbar-btn"
            onClick={(e) => {
              e.preventDefault();
              onNavigate("/workbooks");
            }}
          >
            Workbooks
          </a>
        </div>
      </nav>

      <header className="page-header">
        <div>
          <h1>Sync &amp; CRDT Simulator Lab</h1>
          <p className="page-muted">
            Dedicated <code>reticula-sim-wasm</code> engine loaded on demand: step offline replica
            merges interactively or run the 5-replica fault injector inside your browser.
          </p>
        </div>
        <div className="page-tabs">
          <button
            type="button"
            className={mode === "interactive" ? "page-tab active" : "page-tab"}
            data-testid="sim-mode-interactive"
            onClick={() => {
              stopAutoRunner();
              setMode("interactive");
            }}
          >
            Interactive Replicas
          </button>
          <button
            type="button"
            className={mode === "autorun" ? "page-tab active" : "page-tab"}
            data-testid="sim-mode-autorun"
            onClick={() => setMode("autorun")}
          >
            5-Replica Auto-Run
          </button>
        </div>
      </header>

      {!wasmReady ? (
        <div className="page-card">
          <p>Loading simulator WebAssembly module…</p>
        </div>
      ) : mode === "interactive" ? (
        <>
          {labError && (
            <div className="page-error" role="alert" data-testid="sim-lab-error">
              {labError}
            </div>
          )}

          <div className="sim-controls-bar">
            <div className="collab-row">
              <label htmlFor="sim-replica-count">Replicas:</label>
              <select
                id="sim-replica-count"
                className="collab-select"
                data-testid="sim-replica-count"
                value={replicaCount}
                onChange={(e) => setReplicaCount(e.target.value === "3" ? 3 : 2)}
              >
                <option value="2">2 Replicas (Alice &amp; Bob)</option>
                <option value="3">3 Replicas (Alice, Bob &amp; Carol)</option>
              </select>
              <button
                type="button"
                className="toolbar-btn"
                data-testid="sim-reset-btn"
                onClick={() => resetLab(replicaCount)}
              >
                Reset Lab
              </button>
            </div>
            <div className="collab-row">
              <button
                type="button"
                className="toolbar-btn"
                data-testid="sim-deliver-one"
                disabled={!labState || labState.queue.length === 0}
                onClick={() => runInteractiveOp((lab) => lab.deliverNext())}
              >
                Deliver 1
              </button>
              <button
                type="button"
                className="toolbar-btn primary-btn"
                data-testid="sim-deliver-all"
                disabled={!labState || labState.queue.length === 0}
                onClick={() => runInteractiveOp((lab) => lab.deliverAll())}
              >
                Deliver All ({labState?.queue.length ?? 0})
              </button>
            </div>
          </div>

          {labState && (
            <div className="sim-server-banner">
              <div className="collab-row">
                <strong>Server State</strong>
                <span>headSeq #{labState.serverHeadSeq}</span>
                <span>epoch {labState.serverEpoch}</span>
                <span className="sim-digest-code" title={labState.serverDigest}>
                  digest {labState.serverDigest.slice(0, 16)}…
                </span>
              </div>
              <span
                data-testid="sim-convergence-badge"
                className={
                  labState.allConverged ? "sim-badge sim-badge-ok" : "sim-badge sim-badge-warn"
                }
              >
                {labState.allConverged ? "All Replicas Converged" : "Diverged / Pending Sync"}
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
                  data-testid={`sim-replica-card-${idx}`}
                >
                  <div className="sim-replica-header">
                    <strong>{rep ? `${rep.name} (${rep.role})` : `Replica ${idx + 1}`}</strong>
                    <span
                      data-testid={`sim-replica-status-${idx}`}
                      className={
                        rep?.converged ? "sim-badge sim-badge-ok" : "sim-badge sim-badge-warn"
                      }
                    >
                      {rep?.converged ? "Converged" : (rep?.connection ?? "initializing")}
                    </span>
                  </div>
                  {rep && (
                    <div className="sim-replica-meta">
                      <span>seq #{rep.confirmedSeq}</span>
                      <span>pending {rep.pendingLen}</span>
                      <span>rep {rep.replicaId ?? "—"}</span>
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
                      aria-label={`Replica ${idx} row`}
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
                      aria-label={`Replica ${idx} column`}
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
                      aria-label={`Replica ${idx} cell input`}
                      placeholder="Value or =formula"
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
                      Apply
                    </button>
                  </form>

                  <div className="sim-edit-row">
                    <button
                      type="button"
                      className="toolbar-btn"
                      data-testid={`sim-insert-row-${idx}`}
                      onClick={() => runInteractiveOp((lab) => lab.insertRow(idx, -1))}
                    >
                      + Row Top
                    </button>
                    <button
                      type="button"
                      className="toolbar-btn"
                      data-testid={`sim-undo-${idx}`}
                      disabled={!rep?.canUndo}
                      onClick={() => runInteractiveOp((lab) => lab.undo(idx))}
                    >
                      Undo
                    </button>
                    <select
                      className="collab-select"
                      data-testid={`sim-link-${idx}`}
                      aria-label={`Replica ${idx} network link`}
                      value={rep?.linkMode ?? "online"}
                      onChange={(e) => {
                        const val = e.target.value;
                        runInteractiveOp((lab) => lab.setLinkState(idx, val));
                      }}
                    >
                      <option value="online">Link: Online</option>
                      <option value="delayed">Link: Delayed</option>
                      <option value="dropped">Link: Dropped</option>
                    </select>
                    <button
                      type="button"
                      className="toolbar-btn"
                      data-testid={`sim-crash-${idx}`}
                      onClick={() => runInteractiveOp((lab) => lab.crashAndRestart(idx))}
                    >
                      Crash &amp; Restart
                    </button>
                  </div>
                  {rep?.lastNotice && <p className="page-muted">{rep.lastNotice}</p>}
                </section>
              );
            })}
          </div>

          <section className="page-card" data-testid="sim-wire-inspector">
            <h2>Wire Queue Inspector ({labState?.queue.length ?? 0} frames queued)</h2>
            {!labState || labState.queue.length === 0 ? (
              <p className="page-muted">
                No queued network frames. Switch a replica link to Delayed to hold frames for
                step-by-step inspection.
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
                          <code>{item.direction.toUpperCase()}</code> · {item.summary}
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
              {autoError}
            </div>
          )}

          <div className="sim-edit-row" style={{ marginBottom: "12px" }}>
            <label htmlFor="sim-seed-input">Deterministic Seed:</label>
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
            <button
              type="button"
              className="toolbar-btn primary-btn"
              data-testid="sim-autorun-btn"
              disabled={autoRunning}
              onClick={handleStartAutoRun}
            >
              {autoRunning ? "Running…" : "Run Seed"}
            </button>
            <button
              type="button"
              className="toolbar-btn"
              onClick={() => {
                stopAutoRunner();
                setAutoProgress(null);
                setAutoError(null);
              }}
            >
              Reset
            </button>
          </div>

          <div className="sim-progress-track" style={{ marginBottom: "10px" }}>
            <div className="sim-progress-fill" style={{ width: `${progressPct}%` }} />
          </div>

          {autoProgress && (
            <>
              <div className="sim-server-banner">
                <div className="collab-row">
                  <span>
                    Steps:{" "}
                    <strong>
                      {autoProgress.completedSteps} / {autoProgress.totalSteps}
                    </strong>
                  </span>
                  <span>server headSeq #{autoProgress.headSeq}</span>
                  <span>epoch {autoProgress.epoch}</span>
                  <span className="sim-digest-code" title={autoProgress.serverDigest}>
                    {autoProgress.serverDigest}
                  </span>
                </div>
                <span
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
                      ? "10 Invariants Verified"
                      : "Verification Failed"
                    : "Running…"}
                </span>
              </div>

              <h3>Fault &amp; Operation Telemetry</h3>
              <div className="sim-fault-grid" style={{ marginBottom: "14px" }}>
                <div className="sim-fault-cell">
                  <span className="page-muted">Local edits</span>
                  <strong>{autoProgress.faults.localEdits}</strong>
                </div>
                <div className="sim-fault-cell">
                  <span className="page-muted">SetCellsIf / Formula</span>
                  <strong>
                    {autoProgress.faults.setCellsIfOps} / {autoProgress.faults.formulaOps}
                  </strong>
                </div>
                <div className="sim-fault-cell">
                  <span className="page-muted">Client crashes / OPFS loss</span>
                  <strong>
                    {autoProgress.faults.clientCrashes} / {autoProgress.faults.opfsTailLosses}
                  </strong>
                </div>
                <div className="sim-fault-cell">
                  <span className="page-muted">Server crashes / Epoch bumps</span>
                  <strong>
                    {autoProgress.faults.serverCrashes} / {autoProgress.faults.epochBackupRestores}
                  </strong>
                </div>
                <div className="sim-fault-cell">
                  <span className="page-muted">Snapshot catchups / Rejects</span>
                  <strong>
                    {autoProgress.faults.snapshotRequiredCatchups} /{" "}
                    {autoProgress.faults.terminalRejects}
                  </strong>
                </div>
                <div className="sim-fault-cell">
                  <span className="page-muted">Rogue frames rejected</span>
                  <strong>{autoProgress.faults.rogueAttemptsRejected}</strong>
                </div>
              </div>

              <h3>5 Simulated Collaborators</h3>
              <table className="sim-table" style={{ marginBottom: "14px" }}>
                <thead>
                  <tr>
                    <th>Replica</th>
                    <th>Role</th>
                    <th>Replica ID</th>
                    <th>Connection</th>
                    <th>Confirmed Seq</th>
                    <th>Pending</th>
                  </tr>
                </thead>
                <tbody>
                  {autoProgress.replicas.map((r) => (
                    <tr key={r.index}>
                      <td>#{r.index}</td>
                      <td>{r.role}</td>
                      <td>{r.replica ?? "—"}</td>
                      <td>{r.connection}</td>
                      <td>#{r.confirmedSeq}</td>
                      <td>{r.pendingLen}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </>
          )}

          <h3>Protocol Invariants (1–10)</h3>
          <ul className="sim-invariants-grid" data-testid="sim-invariants-list">
            {TEN_INVARIANTS.map((label) => {
              const status = !autoProgress
                ? "Idle"
                : !autoProgress.finished
                  ? "Checking…"
                  : autoProgress.passed
                    ? "Verified"
                    : "Failed";
              return (
                <li key={label} className="sim-invariant-item">
                  <span>{label}</span>
                  <span
                    className={
                      status === "Verified"
                        ? "sim-badge sim-badge-ok"
                        : status === "Failed"
                          ? "sim-badge sim-badge-warn"
                          : "sim-badge"
                    }
                  >
                    {status}
                  </span>
                </li>
              );
            })}
          </ul>
        </section>
      )}
    </div>
  );
}
