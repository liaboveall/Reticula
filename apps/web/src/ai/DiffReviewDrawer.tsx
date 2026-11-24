import type { DiffHighlightMap } from "@reticula/grid";
import type {
  BranchDiff,
  ClosureIssue,
  ConflictResolution,
  DiffHunk,
  MergePreviewResponse,
  MergeSelection,
} from "@reticula/protocol/collab-api";
import { useEffect, useMemo, useState } from "react";
import { translateEngineText } from "../i18n/engine-text.ts";
import { t } from "../i18n/index.ts";
import { tx } from "../i18n/react.ts";
import { buildHighlights } from "./diff-highlights.ts";
import { closureSource, contentText, hunkKindText } from "./labels.ts";

export interface DiffReviewDrawerProps {
  readonly diff: BranchDiff;
  readonly activeSheetId: number;
  readonly summaryText?: string;
  readonly busy?: boolean;
  readonly errorText?: string | null;
  readonly onPreview: (selection: MergeSelection) => Promise<MergePreviewResponse>;
  readonly onApply: (selection: MergeSelection) => Promise<void>;
  readonly onDiscard: () => Promise<void>;
  readonly onJumpToCell: (sheetId: number, row: number, col: number) => void;
  readonly onSetHighlights: (map: DiffHighlightMap | null) => void;
}

function defaultAcceptedIds(hunks: readonly DiffHunk[]): Set<string> {
  return new Set(
    hunks
      .filter((h) => h.defaultAccepted || (h.hasConflict && h.kind === "cell_block"))
      .map((h) => h.id),
  );
}

export function DiffReviewDrawer({
  diff,
  activeSheetId,
  summaryText,
  busy = false,
  errorText,
  onPreview,
  onApply,
  onDiscard,
  onJumpToCell,
  onSetHighlights,
}: DiffReviewDrawerProps) {
  const [accepted, setAccepted] = useState<Set<string>>(() => defaultAcceptedIds(diff.hunks));
  const [choices, setChoices] = useState<Record<string, ConflictResolution>>({});
  const [allowRef, setAllowRef] = useState(false);
  const [preview, setPreview] = useState<MergePreviewResponse | null>(null);

  useEffect(() => {
    setAccepted(defaultAcceptedIds(diff.hunks));
    setChoices({});
    setAllowRef(false);
  }, [diff]);

  const selection = useMemo<MergeSelection>(
    () => ({
      acceptedHunks: [...accepted],
      conflictChoices: choices,
      allowRefErrors: allowRef,
      expectedMainSeq: diff.mainHeadSeq,
    }),
    [accepted, choices, allowRef, diff.mainHeadSeq],
  );

  useEffect(() => {
    onSetHighlights(buildHighlights(diff.hunks, accepted, activeSheetId));
    return () => onSetHighlights(null);
  }, [diff.hunks, accepted, activeSheetId, onSetHighlights]);

  useEffect(() => {
    let alive = true;
    onPreview(selection)
      .then((res) => {
        if (alive) setPreview(res);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [selection, onPreview]);

  const toggleHunk = (id: string) => {
    setAccepted((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const setChoice = (cellKey: string, res: ConflictResolution) => {
    setChoices((prev) => ({ ...prev, [cellKey]: res }));
  };

  const acceptClosureTargets = (issues: readonly ClosureIssue[]) => {
    setAccepted((prev) => {
      const next = new Set(prev);
      for (const issue of issues) next.add(issue.targetHunkId);
      return next;
    });
  };

  const conflicts = diff.hunks.filter((h) => h.hasConflict);
  const regular = diff.hunks.filter((h) => !h.hasConflict);
  const closureIssues = preview?.closureIssues ?? [];
  const unresolvedCount = preview?.unresolvedConflicts.length ?? 0;
  const canApply = !busy && (preview ? preview.canMerge : unresolvedCount === 0);

  return (
    <div data-testid="ai-diff-review" style={{ display: "flex", flexDirection: "column", gap: 8 }}>
      {summaryText && (
        <div className="ai-card">
          <strong>{t("AI Summary")}</strong>
          <div style={{ marginTop: 4 }}>{summaryText}</div>
        </div>
      )}

      <div className="ai-row">
        <strong>{t("Review Changes ({n} hunks)", { n: diff.hunks.length })}</strong>
        <span className="ai-muted">
          {t("{cells} cells · {conflicts} conflicts", {
            cells: diff.summary.changedCells,
            conflicts: diff.summary.conflictCells,
          })}
        </span>
      </div>

      {conflicts.length > 0 && (
        <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
          <strong style={{ color: "#b45309", fontSize: 12 }}>
            {t("Conflicts ({n}) — Choose Main or AI Draft", { n: conflicts.length })}
          </strong>
          {conflicts.map((h) => (
            <div key={h.id} className="ai-card conflict" data-testid={`diff-hunk-${h.id}`}>
              <div className="ai-row">
                <label style={{ display: "flex", gap: 6, alignItems: "center", fontWeight: 600 }}>
                  <input
                    type="checkbox"
                    checked={accepted.has(h.id)}
                    onChange={() => toggleHunk(h.id)}
                  />
                  {translateEngineText(h.label)}
                </label>
                <span className="ai-tag warn">{t("Conflict")}</span>
              </div>
              {h.conflictReason && (
                <div className="ai-muted" style={{ marginTop: 4, color: "#b45309" }}>
                  {translateEngineText(h.conflictReason)}
                </div>
              )}
              {h.cells.map((c) => (
                <div key={c.cellKey} style={{ marginTop: 6, fontSize: 11 }}>
                  <button
                    type="button"
                    className="ai-diff-jump"
                    onClick={() => onJumpToCell(Number(h.sheetId), c.rowPos, c.colPos)}
                  >
                    {c.sheetName}!{c.address}
                  </button>
                  <div className="ai-muted">
                    {t("Base: {value}", { value: contentText(c.baseFormula, c.baseValue) })}
                  </div>
                  <div>
                    {tx(
                      "<0>Main:</0> {value}",
                      { 0: <strong /> },
                      { value: contentText(c.mainFormula, c.mainValue) },
                    )}
                  </div>
                  <div>
                    {tx(
                      "<0>AI Draft:</0> {value}",
                      { 0: <strong /> },
                      { value: contentText(c.draftFormula, c.draftValue) },
                    )}
                  </div>
                  {h.kind === "cell_block" && (
                    <div className="ai-row" style={{ marginTop: 4, justifyContent: "flex-start" }}>
                      <button
                        type="button"
                        className={`ai-btn ${choices[c.cellKey] === "keep_main" ? "primary" : ""}`}
                        data-testid={`conflict-keep-main-${c.address}`}
                        onClick={() => setChoice(c.cellKey, "keep_main")}
                      >
                        {t("Keep Main")}
                      </button>
                      <button
                        type="button"
                        className={`ai-btn ${choices[c.cellKey] === "take_draft" ? "primary" : ""}`}
                        data-testid={`conflict-take-draft-${c.address}`}
                        onClick={() => setChoice(c.cellKey, "take_draft")}
                      >
                        {t("Use AI Draft")}
                      </button>
                    </div>
                  )}
                </div>
              ))}
            </div>
          ))}
        </div>
      )}

      {regular.map((h) => (
        <div key={h.id} className="ai-card" data-testid={`diff-hunk-${h.id}`}>
          <div className="ai-row">
            <label style={{ display: "flex", gap: 6, alignItems: "center", fontWeight: 600 }}>
              <input
                type="checkbox"
                data-testid={`hunk-check-${h.id}`}
                checked={accepted.has(h.id)}
                onChange={() => toggleHunk(h.id)}
              />
              {translateEngineText(h.label)}
            </label>
            <span className="ai-tag">{hunkKindText(h.kind)}</span>
          </div>
          {h.sortSummary && (
            <div className="ai-muted" style={{ marginTop: 4 }}>
              {t("Sorted {range} ({rows} rows × {cols} cols)", {
                range: h.sortSummary.range,
                rows: h.sortSummary.rowCount,
                cols: h.sortSummary.colCount,
              })}
            </div>
          )}
          {h.cells.slice(0, 6).map((c) => (
            <div key={c.cellKey} className="ai-diff-cell">
              <button
                type="button"
                className="ai-diff-jump"
                onClick={() => onJumpToCell(Number(h.sheetId), c.rowPos, c.colPos)}
              >
                {c.address}
              </button>
              <span className="ai-muted" style={{ overflow: "hidden", textOverflow: "ellipsis" }}>
                {c.baseValue || "∅"} → {c.draftFormula ?? c.draftValue}
              </span>
            </div>
          ))}
          {h.cells.length > 6 && (
            <div className="ai-muted">
              {t("+{n} more cells in this block", { n: h.cells.length - 6 })}
            </div>
          )}
        </div>
      ))}

      {closureIssues.length > 0 && (
        <div className="ai-card alert" data-testid="ai-closure-warning">
          <strong style={{ color: "#dc2626" }}>
            {t("Reference Closure Alert ({n})", { n: closureIssues.length })}
          </strong>
          <div style={{ marginTop: 4, fontSize: 12 }}>
            {t("Accepted formula(s) reference a structural insert you unchecked:")}
          </div>
          {closureIssues.map((iss) => {
            const source = closureSource(iss);
            return (
              <div
                key={`${iss.sourceCell}-${iss.targetHunkId}`}
                style={{ fontSize: 11, marginTop: 3 }}
              >
                <code>{source.cell}</code> ({source.formula}) →{" "}
                <strong>{translateEngineText(iss.targetHunkLabel)}</strong>
              </div>
            );
          })}
          <div className="ai-row" style={{ marginTop: 8 }}>
            <button
              type="button"
              className="ai-btn primary"
              data-testid="ai-closure-accept-together"
              onClick={() => acceptClosureTargets(closureIssues)}
            >
              {t("Accept insert together")}
            </button>
            <label style={{ display: "flex", gap: 4, alignItems: "center", fontSize: 11 }}>
              <input
                type="checkbox"
                data-testid="ai-allow-ref-errors"
                checked={allowRef}
                onChange={(e) => setAllowRef(e.target.checked)}
              />
              {t("Merge with #REF!")}
            </label>
          </div>
        </div>
      )}

      {errorText && (
        <div className="ai-card alert" role="alert">
          {errorText}
        </div>
      )}

      <div className="ai-diff-footer" style={{ padding: 0, borderTop: "none" }}>
        {unresolvedCount > 0 && (
          <div className="ai-muted" style={{ color: "#b45309" }}>
            {t("Resolve {n} conflict(s) above before applying.", { n: unresolvedCount })}
          </div>
        )}
        <div className="ai-row">
          <button
            type="button"
            className="ai-btn primary"
            data-testid="ai-merge-apply"
            disabled={!canApply}
            onClick={() => void onApply(selection)}
          >
            {t("Apply Selected Changes")}
          </button>
          <button
            type="button"
            className="ai-btn danger"
            data-testid="ai-draft-discard"
            disabled={busy}
            onClick={() => void onDiscard()}
          >
            {t("Discard Draft")}
          </button>
        </div>
      </div>
    </div>
  );
}
