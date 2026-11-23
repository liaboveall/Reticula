import type { DiffHighlightKind, DiffHighlightMap } from "@reticula/grid";
import type {
  BranchDiff,
  ClosureIssue,
  ConflictResolution,
  DiffHunk,
  MergePreviewResponse,
  MergeSelection,
} from "@reticula/protocol/collab-api";
import { useEffect, useMemo, useState } from "react";

export interface DiffReviewDrawerProps {
  readonly diff: BranchDiff;
  readonly summaryText?: string;
  readonly busy?: boolean;
  readonly errorText?: string | null;
  readonly onPreview: (selection: MergeSelection) => Promise<MergePreviewResponse>;
  readonly onApply: (selection: MergeSelection) => Promise<void>;
  readonly onDiscard: () => Promise<void>;
  readonly onJumpToCell: (sheetId: number, row: number, col: number) => void;
  readonly onSetHighlights: (map: DiffHighlightMap | null) => void;
}

function buildHighlights(
  hunks: readonly DiffHunk[],
  accepted: ReadonlySet<string>,
): DiffHighlightMap {
  const map = new Map<string, DiffHighlightKind>();
  for (const h of hunks) {
    const isDelete = h.label.toLowerCase().includes("delete");
    for (const c of h.cells) {
      const kind: DiffHighlightKind = c.conflict
        ? "conflict"
        : isDelete
          ? "deleted"
          : accepted.has(h.id)
            ? "added"
            : "deleted";
      map.set(`${c.rowPos}:${c.colPos}`, kind);
    }
  }
  return map;
}

export function DiffReviewDrawer({
  diff,
  summaryText,
  busy = false,
  errorText,
  onPreview,
  onApply,
  onDiscard,
  onJumpToCell,
  onSetHighlights,
}: DiffReviewDrawerProps) {
  const [accepted, setAccepted] = useState<Set<string>>(
    () => new Set(diff.hunks.filter((h) => h.defaultAccepted || h.hasConflict).map((h) => h.id)),
  );
  const [choices, setChoices] = useState<Record<string, ConflictResolution>>({});
  const [allowRef, setAllowRef] = useState(false);
  const [preview, setPreview] = useState<MergePreviewResponse | null>(null);

  useEffect(() => {
    setAccepted(
      new Set(diff.hunks.filter((h) => h.defaultAccepted || h.hasConflict).map((h) => h.id)),
    );
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
    onSetHighlights(buildHighlights(diff.hunks, accepted));
    return () => onSetHighlights(null);
  }, [diff.hunks, accepted, onSetHighlights]);

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
          <strong>AI Summary</strong>
          <div style={{ marginTop: 4 }}>{summaryText}</div>
        </div>
      )}

      <div className="ai-row">
        <strong>Review Changes ({diff.hunks.length} hunks)</strong>
        <span className="ai-muted">
          {diff.summary.changedCells} cells · {diff.summary.conflictCells} conflicts
        </span>
      </div>

      {conflicts.length > 0 && (
        <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
          <strong style={{ color: "#b45309", fontSize: 12 }}>
            Conflicts ({conflicts.length}) — Choose Main or AI Draft
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
                  {h.label}
                </label>
                <span className="ai-tag warn">Conflict</span>
              </div>
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
                    Base: {c.baseFormula ?? (c.baseValue || "(empty)")}
                  </div>
                  <div>
                    <strong>Main:</strong> {c.mainFormula ?? (c.mainValue || "(empty)")}
                  </div>
                  <div>
                    <strong>AI Draft:</strong> {c.draftFormula ?? (c.draftValue || "(empty)")}
                  </div>
                  <div className="ai-row" style={{ marginTop: 4, justifyContent: "flex-start" }}>
                    <button
                      type="button"
                      className={`ai-btn ${choices[c.cellKey] === "keep_main" ? "primary" : ""}`}
                      data-testid={`conflict-keep-main-${c.address}`}
                      onClick={() => setChoice(c.cellKey, "keep_main")}
                    >
                      Keep Main
                    </button>
                    <button
                      type="button"
                      className={`ai-btn ${choices[c.cellKey] === "take_draft" ? "primary" : ""}`}
                      data-testid={`conflict-take-draft-${c.address}`}
                      onClick={() => setChoice(c.cellKey, "take_draft")}
                    >
                      Use AI Draft
                    </button>
                  </div>
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
              {h.label}
            </label>
            <span className="ai-tag">{h.kind}</span>
          </div>
          {h.sortSummary && (
            <div className="ai-muted" style={{ marginTop: 4 }}>
              Sorted {h.sortSummary.range} ({h.sortSummary.rowCount} rows × {h.sortSummary.colCount}{" "}
              cols)
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
            <div className="ai-muted">+{h.cells.length - 6} more cells in this block</div>
          )}
        </div>
      ))}

      {closureIssues.length > 0 && (
        <div className="ai-card alert" data-testid="ai-closure-warning">
          <strong style={{ color: "#dc2626" }}>
            Reference Closure Alert ({closureIssues.length})
          </strong>
          <div style={{ marginTop: 4, fontSize: 12 }}>
            Accepted formula(s) reference a structural insert you unchecked:
          </div>
          {closureIssues.map((iss) => (
            <div
              key={`${iss.sourceCell}-${iss.targetHunkId}`}
              style={{ fontSize: 11, marginTop: 3 }}
            >
              <code>{iss.sourceCell}</code> ({iss.formulaText}) →{" "}
              <strong>{iss.targetHunkLabel}</strong>
            </div>
          ))}
          <div className="ai-row" style={{ marginTop: 8 }}>
            <button
              type="button"
              className="ai-btn primary"
              data-testid="ai-closure-accept-together"
              onClick={() => acceptClosureTargets(closureIssues)}
            >
              Accept insert together
            </button>
            <label style={{ display: "flex", gap: 4, alignItems: "center", fontSize: 11 }}>
              <input
                type="checkbox"
                data-testid="ai-allow-ref-errors"
                checked={allowRef}
                onChange={(e) => setAllowRef(e.target.checked)}
              />
              Merge with #REF!
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
            Resolve {unresolvedCount} conflict(s) above before applying.
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
            Apply Selected Changes
          </button>
          <button
            type="button"
            className="ai-btn danger"
            data-testid="ai-draft-discard"
            disabled={busy}
            onClick={() => void onDiscard()}
          >
            Discard Draft
          </button>
        </div>
      </div>
    </div>
  );
}
