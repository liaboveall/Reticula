import type { DiffHighlightMap, RemotePresenceOverlay } from "@reticula/grid";
import {
  type AgentRunRecord,
  ApiError,
  abandonBranch,
  agentRunEventsUrl,
  type BranchDiff,
  branchDiff,
  cancelAgentRun,
  type ErrorBody,
  getAgentRun,
  listAgentRuns,
  type MergeConflictBody,
  type MergeSelection,
  mergeBranch,
  type PersistedStep,
  previewBranchMerge,
  startAgentRun,
} from "@reticula/protocol/collab-api";
import { type RefObject, useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { personText } from "../collab/labels.ts";
import { translateEngineText } from "../i18n/engine-text.ts";
import { t } from "../i18n/index.ts";
import { ActionButton } from "../ui/ActionButton.tsx";
import { announce } from "../ui/announcer.ts";
import { useDrawer } from "../ui/drawer.ts";
import { focusIsLost } from "../ui/focus.ts";
import { type TabItem, Tabs } from "../ui/Tabs.tsx";
import "./ai.css";
import { DiffReviewDrawer } from "./DiffReviewDrawer.tsx";
import { runStatusText, runSummaryText, stepKindText, stepText } from "./labels.ts";
import {
  buildLocalReplayMergeTxns,
  decodeBase64,
  fetchReplayBundle,
  fetchReplayIndex,
  previewLocalReplayMerge,
  type ReplayBundle,
  type ReplayIndexEntry,
  type ReplayPresenceHint,
} from "./replay-player.ts";

export interface AiPanelProps {
  /** Hidden otherwise, with everything that is going on in it. */
  readonly open: boolean;
  /** What had the focus when it was opened. */
  readonly opener: RefObject<Element | null>;
  readonly onFocusGrid: () => void;
  readonly collabDocId: string | null;
  readonly canWriteCloud: boolean;
  /** Whether nothing that is reviewed may be merged. */
  readonly readOnly: boolean;
  readonly activeSheetId: number;
  readonly draftActive: boolean;
  readonly onClose: () => void;
  readonly onOpenCloudDraft: (docId: string, branchId: string) => void;
  readonly onOpenReplayDraft: (baseSnapshot: ArrayBuffer) => void;
  readonly onApplyReplayTxn: (txn: ArrayBuffer) => void;
  readonly onCommitLocalMerge: (baseSnapshot: ArrayBuffer, txns: ArrayBuffer[]) => void;
  readonly onExitDraft: () => void;
  readonly onSetAiPresence: (presence: RemotePresenceOverlay | null) => void;
  readonly onSetDiffHighlights: (map: DiffHighlightMap | null) => void;
  readonly onJumpToCell: (sheetId: number, row: number, col: number) => void;
}

function presenceFromHint(h: ReplayPresenceHint): RemotePresenceOverlay {
  return {
    replica: 0x00ff_ffff,
    displayName: personText("AI Analyst"),
    colorSlot: 0x80 | 5,
    cursorRow: h.row,
    cursorCol: h.col,
    selection:
      h.endRow > h.row || h.endCol > h.col
        ? {
            firstRow: h.row,
            firstCol: h.col,
            endRow: h.endRow + 1,
            endCol: h.endCol + 1,
          }
        : null,
  };
}

function failureText(err: unknown): string {
  if (err instanceof ApiError) {
    return t("Reticula API returned {status}", { status: err.status });
  }
  return err instanceof Error ? translateEngineText(err.message) : String(err);
}

type TabKey = "cloud" | "demos";

export default function AiPanel({
  open,
  opener,
  onFocusGrid,
  collabDocId,
  canWriteCloud,
  readOnly,
  activeSheetId,
  draftActive,
  onClose,
  onOpenCloudDraft,
  onOpenReplayDraft,
  onApplyReplayTxn,
  onCommitLocalMerge,
  onExitDraft,
  onSetAiPresence,
  onSetDiffHighlights,
  onJumpToCell,
}: AiPanelProps) {
  const cloudAvailable = collabDocId !== null && canWriteCloud;
  const [tab, setTab] = useState<TabKey>(cloudAvailable ? "cloud" : "demos");
  const shownTab = cloudAvailable ? tab : "demos";
  const [followCursor, setFollowCursor] = useState(true);
  const followCursorRef = useRef(followCursor);
  followCursorRef.current = followCursor;
  const prevDraftActiveRef = useRef(draftActive);

  // Cloud mode state
  const [prompt, setPrompt] = useState("");
  const [runs, setRuns] = useState<AgentRunRecord[]>([]);
  const [activeRun, setActiveRun] = useState<AgentRunRecord | null>(null);
  const [steps, setSteps] = useState<PersistedStep[]>([]);
  const [streamText, setStreamText] = useState("");
  const [cloudDiff, setCloudDiff] = useState<BranchDiff | null>(null);
  const [cloudError, setCloudError] = useState<string | null>(null);
  const [cloudBusy, setCloudBusy] = useState(false);
  const sseRef = useRef<EventSource | null>(null);

  // Local replay mode state
  const [demos, setDemos] = useState<readonly ReplayIndexEntry[]>([]);
  const [bundle, setBundle] = useState<ReplayBundle | null>(null);
  const [replayIdx, setReplayIdx] = useState(-1);
  const [playing, setPlaying] = useState(false);
  const [replayReview, setReplayReview] = useState(false);
  const [replayMsg, setReplayMsg] = useState<string | null>(null);

  const headingRef = useRef<HTMLHeadingElement>(null);
  const promptRef = useRef<HTMLTextAreaElement>(null);
  const playRef = useRef<HTMLButtonElement>(null);
  const demoListRef = useRef<HTMLDivElement>(null);
  const firstRef = useRef<HTMLElement>(null);
  useLayoutEffect(() => {
    firstRef.current = promptRef.current ?? headingRef.current;
  });
  const panelRef = useDrawer({ open, initialFocus: firstRef, opener, onLeave: onFocusGrid });

  // What was pressed to get to a recording or back to the list is gone then.
  const shownDemoRef = useRef<string | null>(null);
  const bundleId = bundle?.id ?? null;
  useLayoutEffect(() => {
    const shownBefore = shownDemoRef.current;
    shownDemoRef.current = bundleId;
    if (bundleId === shownBefore || !focusIsLost()) {
      return;
    }
    if (bundleId !== null) {
      playRef.current?.focus();
    } else {
      demoListRef.current
        ?.querySelector<HTMLElement>(`[data-testid="ai-demo-${shownBefore}"]`)
        ?.focus();
    }
  }, [bundleId]);

  const runStatus = activeRun?.status;
  useEffect(() => {
    if (runStatus !== undefined) {
      announce(t("Status: {status}", { status: runStatusText(runStatus) }));
    }
  }, [runStatus]);

  useEffect(() => {
    if (replayMsg !== null) {
      announce(replayMsg);
    }
  }, [replayMsg]);

  const reviewedHunks = replayReview ? bundle?.diff.hunks.length : cloudDiff?.hunks.length;
  useEffect(() => {
    if (reviewedHunks !== undefined) {
      announce(t("Review Changes ({n} hunks)", { n: reviewedHunks }));
    }
  }, [reviewedHunks]);

  const closeSse = useCallback(() => {
    sseRef.current?.close();
    sseRef.current = null;
  }, []);

  // The panel stays when another workbook is opened; what it shows of a cloud
  // workbook's runs does not, nor does what was still being waited for.
  const runsOfRef = useRef(collabDocId);
  useEffect(() => {
    if (runsOfRef.current === collabDocId) return;
    runsOfRef.current = collabDocId;
    closeSse();
    setTab(cloudAvailable && shownDemoRef.current === null ? "cloud" : "demos");
    setRuns([]);
    setActiveRun(null);
    setSteps([]);
    setStreamText("");
    setCloudDiff(null);
    setCloudError(null);
  }, [collabDocId, cloudAvailable, closeSse]);

  const refreshRuns = useCallback((docId: string) => {
    listAgentRuns(docId)
      .then((list) => {
        if (runsOfRef.current === docId) setRuns(list);
      })
      .catch(() => {});
  }, []);

  useEffect(() => {
    if (open && collabDocId) refreshRuns(collabDocId);
  }, [open, collabDocId, refreshRuns]);

  // What could not be loaded is asked for again when the panel is opened again.
  useEffect(() => {
    if (!open || shownTab !== "demos" || demos.length > 0) return;
    fetchReplayIndex()
      .then((idx) => {
        setDemos(idx.items);
        setReplayMsg(null);
      })
      .catch((err) => setReplayMsg(failureText(err)));
  }, [open, shownTab, demos.length]);

  useEffect(() => () => closeSse(), [closeSse]);

  // If the user clicks "Back to Main" on the editor banner, clear any open review drawer & SSE stream.
  useEffect(() => {
    const wasActive = prevDraftActiveRef.current;
    prevDraftActiveRef.current = draftActive;
    if (wasActive && !draftActive) {
      closeSse();
      setPlaying(false);
      setCloudDiff(null);
      setReplayReview(false);
      onSetAiPresence(null);
      onSetDiffHighlights(null);
    }
  }, [closeSse, draftActive, onSetAiPresence, onSetDiffHighlights]);

  const loadCloudReview = useCallback(async (docId: string, branchId: string) => {
    try {
      const d = await branchDiff(docId, branchId);
      if (runsOfRef.current === docId) setCloudDiff(d);
    } catch (err) {
      if (runsOfRef.current === docId) setCloudError(failureText(err));
    }
  }, []);

  const attachSse = useCallback(
    (docId: string, run: AgentRunRecord) => {
      closeSse();
      setActiveRun(run);
      setSteps([]);
      setStreamText("");
      setCloudDiff(null);
      setCloudError(null);
      onOpenCloudDraft(docId, run.branchId);

      const es = new EventSource(agentRunEventsUrl(docId, run.id), { withCredentials: true });
      sseRef.current = es;
      es.addEventListener("step", (ev) => {
        try {
          const step = JSON.parse((ev as MessageEvent).data) as PersistedStep;
          setSteps((prev) => (prev.some((s) => s.idx === step.idx) ? prev : [...prev, step]));
        } catch {
          // ignore malformed event
        }
      });
      es.addEventListener("text_delta", (ev) => {
        try {
          const d = JSON.parse((ev as MessageEvent).data) as { text?: string };
          if (d.text) setStreamText((prev) => prev + d.text);
        } catch {
          // ignore
        }
      });
      es.addEventListener("presence", (ev) => {
        try {
          const h = JSON.parse((ev as MessageEvent).data) as ReplayPresenceHint;
          onSetAiPresence(presenceFromHint(h));
          if (followCursorRef.current) onJumpToCell(h.sheet, h.row, h.col);
        } catch {
          // ignore
        }
      });
      es.addEventListener("status", (ev) => {
        try {
          const d = JSON.parse((ev as MessageEvent).data) as {
            status: string;
            summary?: string;
            error?: string;
            commitsWritten?: number;
          };
          setActiveRun((prev) =>
            prev
              ? {
                  ...prev,
                  status: d.status,
                  summary: d.summary ?? prev.summary,
                  errorMessage: d.error ?? prev.errorMessage ?? null,
                }
              : prev,
          );
          if (d.status === "waiting_review") {
            if (d.commitsWritten === 0) {
              // Read-only answer with no draft edits: exit preview and display summary.
              onExitDraft();
            } else {
              void loadCloudReview(docId, run.branchId);
            }
          } else if (d.error) {
            setCloudError(translateEngineText(d.error));
          }
          es.close();
          if (sseRef.current === es) sseRef.current = null;
          onSetAiPresence(null);
          refreshRuns(docId);
        } catch {
          // ignore
        }
      });
    },
    [
      closeSse,
      loadCloudReview,
      onExitDraft,
      onJumpToCell,
      onOpenCloudDraft,
      onSetAiPresence,
      refreshRuns,
    ],
  );

  const canRun = !cloudBusy && prompt.trim() !== "" && activeRun?.status !== "running";

  const handleStartRun = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!collabDocId || !canRun) return;
    setCloudBusy(true);
    setCloudError(null);
    try {
      const run = await startAgentRun(collabDocId, { prompt: prompt.trim() });
      setPrompt("");
      if (runsOfRef.current === collabDocId) attachSse(collabDocId, run);
    } catch (err) {
      if (runsOfRef.current !== collabDocId) return;
      if (err instanceof ApiError && err.status === 503) {
        setCloudError(
          t(
            "Cloud AI is not configured on this server yet. Try the 10 built-in interactive demos below.",
          ),
        );
      } else if (err instanceof ApiError && err.body && typeof err.body === "object") {
        const { error } = err.body as Partial<ErrorBody>;
        setCloudError(typeof error === "string" ? translateEngineText(error) : failureText(err));
      } else {
        setCloudError(failureText(err));
      }
    } finally {
      setCloudBusy(false);
    }
  };

  const handleStopRun = async () => {
    if (!collabDocId || !activeRun) return;
    await cancelAgentRun(collabDocId, activeRun.id).catch(() => {});
    closeSse();
    onSetAiPresence(null);
  };

  const handleOpenExistingRun = async (r: AgentRunRecord) => {
    if (!collabDocId) return;
    setCloudError(null);
    const detail = await getAgentRun(collabDocId, r.id);
    if (runsOfRef.current !== collabDocId) return;
    setActiveRun(detail.run);
    setSteps(detail.steps);
    if (detail.run.status === "running" || detail.run.status === "queued") {
      attachSse(collabDocId, detail.run);
    } else if (detail.run.status === "waiting_review") {
      const hasCommits = detail.steps.some((s) => s.kind === "txn_commit");
      if (hasCommits) {
        onOpenCloudDraft(collabDocId, detail.run.branchId);
        await loadCloudReview(collabDocId, detail.run.branchId);
      }
    }
  };

  const handleCloudPreview = useCallback(
    async (sel: MergeSelection) => {
      if (!collabDocId || !activeRun) {
        return {
          canMerge: false,
          mainHeadSeq: "0",
          closureIssues: [],
          unresolvedConflicts: [],
          estimatedOps: 0,
        };
      }
      return previewBranchMerge(collabDocId, activeRun.branchId, sel);
    },
    [collabDocId, activeRun],
  );

  const handleCloudApply = async (sel: MergeSelection) => {
    if (!collabDocId || !activeRun) return;
    setCloudBusy(true);
    setCloudError(null);
    try {
      await mergeBranch(collabDocId, activeRun.branchId, sel);
      if (runsOfRef.current !== collabDocId) return;
      setCloudDiff(null);
      setActiveRun((prev) => (prev ? { ...prev, status: "merged" } : prev));
      onSetDiffHighlights(null);
      onSetAiPresence(null);
      onExitDraft();
      refreshRuns(collabDocId);
    } catch (err) {
      if (runsOfRef.current !== collabDocId) return;
      if (
        err instanceof ApiError &&
        err.status === 409 &&
        err.body &&
        typeof err.body === "object"
      ) {
        const conflict = err.body as Partial<MergeConflictBody>;
        if (conflict.diff) setCloudDiff(conflict.diff);
        setCloudError(
          typeof conflict.error === "string"
            ? translateEngineText(conflict.error)
            : t("Merge conflict detected; review updated diff."),
        );
      } else {
        setCloudError(failureText(err));
      }
    } finally {
      setCloudBusy(false);
    }
  };

  const handleCloudDiscard = async () => {
    if (!collabDocId || !activeRun) return;
    setCloudBusy(true);
    try {
      await abandonBranch(collabDocId, activeRun.branchId);
      if (runsOfRef.current !== collabDocId) return;
      setCloudDiff(null);
      setActiveRun((prev) => (prev ? { ...prev, status: "rejected" } : prev));
      onSetDiffHighlights(null);
      onSetAiPresence(null);
      onExitDraft();
      refreshRuns(collabDocId);
    } finally {
      setCloudBusy(false);
    }
  };

  // Replay stepping logic
  const selectDemo = async (id: string) => {
    setPlaying(false);
    setReplayReview(false);
    setReplayMsg(null);
    onSetDiffHighlights(null);
    onSetAiPresence(null);
    try {
      const loaded = await fetchReplayBundle(id);
      setBundle(loaded);
      setReplayIdx(-1);
      onOpenReplayDraft(decodeBase64(loaded.baseSnapshotB64));
    } catch (err) {
      setReplayMsg(failureText(err));
    }
  };

  const stepReplayOnce = useCallback(() => {
    if (!bundle) return false;
    const nextIdx = replayIdx + 1;
    if (nextIdx >= bundle.steps.length) {
      setPlaying(false);
      setReplayReview(true);
      onSetAiPresence(null);
      return false;
    }
    const step = bundle.steps[nextIdx];
    if (!step) return false;
    setReplayIdx(nextIdx);
    if (step.txnB64) {
      onApplyReplayTxn(decodeBase64(step.txnB64));
    }
    if (step.presence) {
      onSetAiPresence(presenceFromHint(step.presence));
      if (followCursor) onJumpToCell(step.presence.sheet, step.presence.row, step.presence.col);
    }
    if (nextIdx === bundle.steps.length - 1) {
      setPlaying(false);
      setReplayReview(true);
      onSetAiPresence(null);
    } else if (!playing) {
      // While it plays, the steps follow one another faster than they can be read.
      announce(`${nextIdx + 1} / ${bundle.steps.length}: ${step.title}`);
    }
    return true;
  }, [bundle, followCursor, onApplyReplayTxn, onJumpToCell, onSetAiPresence, playing, replayIdx]);

  useEffect(() => {
    if (!playing || !bundle) return;
    const timer = window.setTimeout(() => {
      stepReplayOnce();
    }, 260);
    return () => window.clearTimeout(timer);
  }, [playing, bundle, stepReplayOnce]);

  const handleReplayPreview = useCallback(
    async (sel: MergeSelection) => {
      if (!bundle) {
        return {
          canMerge: false,
          mainHeadSeq: "0",
          closureIssues: [],
          unresolvedConflicts: [],
          estimatedOps: 0,
        };
      }
      return previewLocalReplayMerge(
        bundle,
        new Set(sel.acceptedHunks),
        sel.conflictChoices ?? {},
        Boolean(sel.allowRefErrors),
      );
    },
    [bundle],
  );

  const handleReplayApply = async (sel: MergeSelection) => {
    if (!bundle) return;
    onSetDiffHighlights(null);
    onSetAiPresence(null);
    if (collabDocId !== null || readOnly) {
      onExitDraft();
      setReplayReview(false);
      setReplayMsg(
        readOnly
          ? t('Previewed "{title}". Nothing was merged: the workbook is read-only here.', {
              title: bundle.title,
            })
          : t(
              'Previewed "{title}" merge selection; returned to your cloud workbook without overwriting it.',
              { title: bundle.title },
            ),
      );
      return;
    }
    const txns = buildLocalReplayMergeTxns(
      bundle,
      new Set(sel.acceptedHunks),
      sel.conflictChoices ?? {},
      Boolean(sel.allowRefErrors),
    );
    onCommitLocalMerge(decodeBase64(bundle.baseSnapshotB64), txns);
    setReplayReview(false);
    setReplayMsg(
      t('Merged selected changes from "{title}" into your sheet.', { title: bundle.title }),
    );
  };

  const handleReplayDiscard = async () => {
    setPlaying(false);
    setReplayReview(false);
    setBundle(null);
    setReplayIdx(-1);
    onSetDiffHighlights(null);
    onSetAiPresence(null);
    onExitDraft();
  };

  const tabs: TabItem<TabKey>[] = [
    { key: "demos", text: t("10 Recorded Demos"), testId: "ai-tab-demos" },
  ];
  if (cloudAvailable) {
    tabs.unshift({ key: "cloud", text: t("Cloud Draft"), testId: "ai-tab-cloud" });
  }

  return (
    <aside
      ref={panelRef}
      className="ai-panel"
      data-testid="ai-panel"
      aria-label={t("AI Analyst")}
      hidden={!open}
    >
      <div className="ai-panel-head">
        <h2 ref={headingRef} className="ai-panel-title" tabIndex={-1}>
          <span className="ai-badge">{t("AI")}</span>
          <span>{t("Analyst")}</span>
        </h2>
        <div className="ai-row">
          <label style={{ display: "flex", gap: 4, alignItems: "center", fontSize: 11 }}>
            <input
              type="checkbox"
              checked={followCursor}
              onChange={(e) => setFollowCursor(e.target.checked)}
            />
            {t("Follow cursor")}
          </label>
          <button
            type="button"
            className="ai-btn"
            aria-label={t("Close")}
            data-testid="ai-close-btn"
            onClick={onClose}
          >
            ✕
          </button>
        </div>
      </div>

      <Tabs
        label={t("AI Analyst")}
        tabs={tabs}
        selected={shownTab}
        onSelect={setTab}
        listClassName="ai-tabs"
        tabClassName="ai-tab"
        panelClassName="ai-panel-body"
      >
        {shownTab === "cloud" ? (
          <>
            <form className="ai-form" onSubmit={handleStartRun}>
              <textarea
                ref={promptRef}
                className="ai-textarea"
                aria-label={t("Prompt for the AI Analyst")}
                data-testid="ai-prompt-input"
                placeholder={t(
                  "Ask AI Analyst to inspect, build formulas, audit errors, or sort on an isolated draft branch...",
                )}
                value={prompt}
                onChange={(e) => setPrompt(e.target.value)}
                maxLength={4000}
              />
              <div className="ai-row">
                <ActionButton
                  type="submit"
                  className="ai-btn primary"
                  data-testid="ai-run-btn"
                  inactive={!canRun}
                >
                  {t("Run on Draft Branch")}
                </ActionButton>
                {activeRun?.status === "running" && (
                  <button
                    type="button"
                    className="ai-btn danger"
                    data-testid="ai-stop-btn"
                    onClick={() => void handleStopRun()}
                  >
                    {t("Stop")}
                  </button>
                )}
              </div>
            </form>

            {cloudError && !cloudDiff && (
              <div className="ai-card alert" role="alert">
                {cloudError}
              </div>
            )}

            {activeRun && (
              <div className="ai-card" data-testid="ai-run-status">
                <div className="ai-row">
                  <strong>
                    {t("Status: {status}", { status: runStatusText(activeRun.status) })}
                  </strong>
                  <span className="ai-muted">{activeRun.model}</span>
                </div>
                {streamText && (
                  <div className="ai-muted" style={{ marginTop: 4 }}>
                    {streamText.slice(-240)}
                  </div>
                )}
              </div>
            )}

            {steps.length > 0 && !cloudDiff && (
              <div className="ai-step-list" data-testid="ai-step-list">
                {steps.map((s) => (
                  <div key={s.idx} className="ai-step">
                    <div className="ai-step-kind">{stepKindText(s.kind)}</div>
                    <div>{stepText(s.payload)}</div>
                  </div>
                ))}
              </div>
            )}

            {cloudDiff && activeRun && (
              <DiffReviewDrawer
                diff={cloudDiff}
                activeSheetId={activeSheetId}
                summaryText={runSummaryText(activeRun.summary)}
                busy={cloudBusy}
                errorText={cloudError}
                onPreview={handleCloudPreview}
                onApply={handleCloudApply}
                onDiscard={handleCloudDiscard}
                onJumpToCell={onJumpToCell}
                onSetHighlights={onSetDiffHighlights}
              />
            )}

            {runs.length > 0 && !cloudDiff && (
              <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
                <h3 className="ai-heading">{t("Recent Runs")}</h3>
                {runs.slice(0, 5).map((r) => (
                  <button
                    key={r.id}
                    type="button"
                    className="ai-replay-item"
                    onClick={() => void handleOpenExistingRun(r)}
                  >
                    <div className="ai-row">
                      <strong>{runStatusText(r.status)}</strong>
                      <span className="ai-muted">{t("{n} steps", { n: r.stepCount })}</span>
                    </div>
                    <span className="ai-muted">{r.prompt.slice(0, 80)}</span>
                  </button>
                ))}
              </div>
            )}
          </>
        ) : (
          <>
            <div className="ai-muted">
              {t(
                "Pure-client interactive replays running the real WASM FormulaEngine. No sign-in or API key required.",
              )}
            </div>
            {replayMsg && <div className="ai-card">{replayMsg}</div>}

            {!bundle ? (
              <div
                ref={demoListRef}
                style={{ display: "flex", flexDirection: "column", gap: 6 }}
                data-testid="ai-demo-list"
              >
                {demos.map((d) => (
                  <button
                    key={d.id}
                    type="button"
                    className="ai-replay-item"
                    data-testid={`ai-demo-${d.id}`}
                    onClick={() => void selectDemo(d.id)}
                  >
                    <strong>{d.title}</strong>
                    <span className="ai-muted">{d.description}</span>
                    <div className="ai-tags">
                      {d.hasConflictDemo && (
                        <span className="ai-tag warn">{t("3-Way Conflict")}</span>
                      )}
                      {d.hasClosureDemo && (
                        <span className="ai-tag warn">{t("Closure Guard")}</span>
                      )}
                      {d.tags.map((tag) => (
                        <span key={tag} className="ai-tag">
                          {tag}
                        </span>
                      ))}
                    </div>
                  </button>
                ))}
              </div>
            ) : (
              <>
                <div className="ai-card">
                  <div className="ai-row">
                    <strong>{bundle.title}</strong>
                    <button
                      type="button"
                      className="ai-btn"
                      onClick={() => void handleReplayDiscard()}
                    >
                      {t("All Demos")}
                    </button>
                  </div>
                  <div className="ai-muted" style={{ marginTop: 4 }}>
                    {t("Prompt: “{prompt}”", { prompt: bundle.prompt })}
                  </div>
                  <div className="ai-row" style={{ marginTop: 8 }}>
                    <button
                      ref={playRef}
                      type="button"
                      className="ai-btn primary"
                      data-testid="ai-replay-play"
                      onClick={() => {
                        if (replayIdx >= bundle.steps.length - 1) {
                          setReplayReview(true);
                        } else {
                          setPlaying((p) => !p);
                        }
                      }}
                    >
                      {playing ? t("Pause") : replayIdx < 0 ? t("Play Replay") : t("Resume")}
                    </button>
                    <ActionButton
                      className="ai-btn"
                      data-testid="ai-replay-step"
                      inactive={playing}
                      onClick={() => void stepReplayOnce()}
                    >
                      {t("Step ({done}/{total})", {
                        done: Math.max(0, replayIdx + 1),
                        total: bundle.steps.length,
                      })}
                    </ActionButton>
                    <button
                      type="button"
                      className="ai-btn"
                      data-testid="ai-replay-review-btn"
                      onClick={() => {
                        setPlaying(false);
                        // Fast-forward remaining txns into draft engine before opening diff review.
                        for (let i = replayIdx + 1; i < bundle.steps.length; i += 1) {
                          const st = bundle.steps[i];
                          if (st?.txnB64) onApplyReplayTxn(decodeBase64(st.txnB64));
                        }
                        setReplayIdx(bundle.steps.length - 1);
                        onSetAiPresence(null);
                        setReplayReview(true);
                      }}
                    >
                      {t("Review Changes")}
                    </button>
                  </div>
                </div>

                {!replayReview && replayIdx >= 0 && (
                  <div className="ai-step-list" data-testid="ai-replay-steps">
                    {bundle.steps.slice(0, replayIdx + 1).map((s) => (
                      <div key={s.idx} className="ai-step">
                        <div className="ai-step-kind">{s.title}</div>
                        <div>{s.detail}</div>
                      </div>
                    ))}
                  </div>
                )}

                {replayReview && (
                  <DiffReviewDrawer
                    diff={bundle.diff}
                    activeSheetId={activeSheetId}
                    summaryText={bundle.summary}
                    onPreview={handleReplayPreview}
                    onApply={handleReplayApply}
                    onDiscard={handleReplayDiscard}
                    onJumpToCell={onJumpToCell}
                    onSetHighlights={onSetDiffHighlights}
                  />
                )}
              </>
            )}
          </>
        )}
      </Tabs>
    </aside>
  );
}
