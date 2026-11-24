import {
  createVersion,
  deleteVersion,
  type HistoryEntry,
  history,
  listVersions,
  type VersionEntry,
} from "@reticula/protocol/collab-api";
import { type FormEvent, useCallback, useEffect, useState } from "react";
import { translateEngineText } from "../i18n/engine-text.ts";
import { fmtDateTime, t } from "../i18n/index.ts";
import { apiErrorMessage, pickNearestSnapshotSeq } from "../pages/helpers.ts";
import { personText } from "./labels.ts";

export interface HistoryDrawerProps {
  readonly docId: string;
  readonly shareToken?: string | null | undefined;
  readonly readOnly: boolean;
  readonly open: boolean;
  readonly previewSeq: string | null;
  readonly previewElapsedMs: number | null;
  readonly onSeek: (targetSeq: string, snapshotSeq: string | null) => void;
  readonly onExit: () => void;
  readonly onClose: () => void;
}

function formatTimestamp(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) {
    return iso;
  }
  return fmtDateTime(date);
}

export function HistoryDrawer(props: HistoryDrawerProps) {
  const {
    docId,
    shareToken,
    readOnly,
    open,
    previewSeq,
    previewElapsedMs,
    onSeek,
    onExit,
    onClose,
  } = props;

  const [headSeq, setHeadSeq] = useState("0");
  const [snapshotSeqs, setSnapshotSeqs] = useState<string[]>([]);
  const [entries, setEntries] = useState<HistoryEntry[]>([]);
  const [versions, setVersions] = useState<VersionEntry[]>([]);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [versionName, setVersionName] = useState("");

  const loadHistory = useCallback(
    async (signal?: AbortSignal) => {
      setLoading(true);
      setError(null);
      try {
        const historyOpts = shareToken
          ? { limit: 100, xReticulaShare: shareToken }
          : { limit: 100 };
        const versionOpts = shareToken ? { xReticulaShare: shareToken } : undefined;
        const [page, versionList] = await Promise.all([
          history(docId, signal, historyOpts),
          listVersions(docId, signal, versionOpts),
        ]);
        if (signal?.aborted) {
          return;
        }
        setHeadSeq(page.headSeq);
        setSnapshotSeqs(page.snapshotSeqs);
        setEntries(page.entries);
        setVersions(versionList);
      } catch (err) {
        if (!signal?.aborted) {
          setError(apiErrorMessage(err, t("Failed to load workbook history.")));
        }
      } finally {
        if (!signal?.aborted) {
          setLoading(false);
        }
      }
    },
    [docId, shareToken],
  );

  useEffect(() => {
    if (!open) {
      return;
    }
    const controller = new AbortController();
    void loadHistory(controller.signal);
    return () => {
      controller.abort();
    };
  }, [open, loadHistory]);

  if (!open) {
    return null;
  }

  const headBig = BigInt(headSeq || "0");
  const activeSeq = previewSeq ?? headSeq;
  const activeBig = BigInt(activeSeq || "0");
  const oldestLoaded = entries.length > 0 ? entries[entries.length - 1] : undefined;
  const canLoadOlder = oldestLoaded !== undefined && BigInt(oldestLoaded.seq) > 1n;

  const seekSeq = (targetSeq: string) => {
    const snapSeq = pickNearestSnapshotSeq(snapshotSeqs, targetSeq);
    onSeek(targetSeq, snapSeq);
  };

  const handleStep = (delta: bigint) => {
    const next = activeBig + delta;
    const clamped = next < 0n ? 0n : next > headBig ? headBig : next;
    seekSeq(clamped.toString());
  };

  const handleLoadOlder = async () => {
    if (!oldestLoaded) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const opts = shareToken
        ? { beforeSeq: oldestLoaded.seq, limit: 100, xReticulaShare: shareToken }
        : { beforeSeq: oldestLoaded.seq, limit: 100 };
      const page = await history(docId, undefined, opts);
      setHeadSeq(page.headSeq);
      setSnapshotSeqs(page.snapshotSeqs);
      setEntries((prev) => {
        const seen = new Set(prev.map((item) => item.seq));
        return [...prev, ...page.entries.filter((item) => !seen.has(item.seq))];
      });
    } catch (err) {
      setError(apiErrorMessage(err, t("Failed to load older history entries.")));
    } finally {
      setBusy(false);
    }
  };

  const handleCreateVersion = async (event: FormEvent) => {
    event.preventDefault();
    if (readOnly) {
      return;
    }
    const trimmed = versionName.trim();
    if (!trimmed || activeSeq === "0") {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const created = await createVersion(docId, { seq: activeSeq, name: trimmed });
      setVersions((prev) => [created, ...prev]);
      setVersionName("");
    } catch (err) {
      setError(apiErrorMessage(err, t("Failed to save named version.")));
    } finally {
      setBusy(false);
    }
  };

  const handleDeleteVersion = async (versionId: string) => {
    if (readOnly) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await deleteVersion(docId, versionId);
      setVersions((prev) => prev.filter((item) => item.id !== versionId));
    } catch (err) {
      setError(apiErrorMessage(err, t("Failed to delete named version.")));
    } finally {
      setBusy(false);
    }
  };

  const headMax = Number(
    headBig <= BigInt(Number.MAX_SAFE_INTEGER) ? headBig : BigInt(Number.MAX_SAFE_INTEGER),
  );
  const sliderVal = Number(
    activeBig <= BigInt(Number.MAX_SAFE_INTEGER) ? activeBig : BigInt(Number.MAX_SAFE_INTEGER),
  );

  return (
    <aside
      className="history-drawer"
      data-testid="history-drawer"
      aria-label={t("Version History")}
    >
      <div className="history-drawer-header">
        <div>
          <h3 className="history-drawer-title">{t("Version History")}</h3>
          <div className="history-drawer-subtitle">
            {previewElapsedMs === null
              ? t("Revision #{seq} of #{head}", { seq: activeSeq, head: headSeq })
              : t("Revision #{seq} of #{head} ({ms} ms)", {
                  seq: activeSeq,
                  head: headSeq,
                  ms: Math.round(previewElapsedMs),
                })}
          </div>
        </div>
        <div className="history-drawer-header-actions">
          {previewSeq !== null && (
            <button
              type="button"
              className="toolbar-btn primary-btn"
              data-testid="history-return-live-btn"
              onClick={onExit}
            >
              {t("Return to Live")}
            </button>
          )}
          <button
            type="button"
            className="toolbar-btn"
            data-testid="history-refresh-btn"
            disabled={loading || busy}
            onClick={() => void loadHistory()}
          >
            {t("Refresh")}
          </button>
          <button
            type="button"
            className="toolbar-btn"
            data-testid="history-close-btn"
            onClick={onClose}
          >
            {t("Close")}
          </button>
        </div>
      </div>

      {error && (
        <div className="collab-error" role="alert" data-testid="history-error">
          {error}
        </div>
      )}

      <div className="history-scrubber-box">
        <div className="collab-row">
          <button
            type="button"
            className="toolbar-btn"
            data-testid="history-step-prev"
            disabled={activeBig <= 0n || loading}
            onClick={() => handleStep(-1n)}
          >
            -1
          </button>
          <input
            type="range"
            className="history-range"
            data-testid="history-scrubber"
            aria-label={t("History sequence scrubber")}
            min={0}
            max={headMax}
            step={1}
            disabled={headMax === 0 || loading}
            value={Math.min(Math.max(sliderVal, 0), headMax)}
            onChange={(event) => seekSeq(event.target.value)}
          />
          <button
            type="button"
            className="toolbar-btn"
            data-testid="history-step-next"
            disabled={activeBig >= headBig || loading}
            onClick={() => handleStep(1n)}
          >
            +1
          </button>
        </div>
      </div>

      {!readOnly && (
        <form
          className="history-version-form"
          onSubmit={(event) => void handleCreateVersion(event)}
        >
          <input
            type="text"
            className="collab-input"
            data-testid="history-version-name-input"
            placeholder={t("Save #{seq} as version...", { seq: activeSeq })}
            maxLength={120}
            value={versionName}
            disabled={busy || activeSeq === "0"}
            onChange={(event) => setVersionName(event.target.value)}
          />
          <button
            type="submit"
            className="toolbar-btn primary-btn"
            data-testid="history-save-version-btn"
            disabled={busy || activeSeq === "0" || versionName.trim().length === 0}
          >
            {t("Save")}
          </button>
        </form>
      )}

      <div className="history-section">
        <h4 className="collab-section-title">
          {t("Named Versions ({n})", { n: versions.length })}
        </h4>
        {versions.length === 0 ? (
          <p className="history-empty">{t("No named versions saved yet.")}</p>
        ) : (
          <ul className="history-list" data-testid="history-versions-list">
            {versions.map((ver) => (
              <li
                key={ver.id}
                className={
                  previewSeq === ver.seq ? "history-item history-item-active" : "history-item"
                }
              >
                <button
                  type="button"
                  className="history-item-btn"
                  data-testid={`history-version-${ver.seq}`}
                  onClick={() => seekSeq(ver.seq)}
                >
                  <span className="history-item-title">{ver.name}</span>
                  <span className="history-item-meta">
                    #{ver.seq} · {personText(ver.createdByName)} · {formatTimestamp(ver.createdAt)}
                  </span>
                </button>
                {!readOnly && (
                  <button
                    type="button"
                    className="toolbar-btn"
                    data-testid={`history-delete-version-${ver.id}`}
                    disabled={busy}
                    onClick={() => void handleDeleteVersion(ver.id)}
                  >
                    {t("Delete")}
                  </button>
                )}
              </li>
            ))}
          </ul>
        )}
      </div>

      <div className="history-section history-section-scroll">
        <h4 className="collab-section-title">{t("Revisions ({n})", { n: entries.length })}</h4>
        {loading && entries.length === 0 ? (
          <p className="history-empty">{t("Loading history...")}</p>
        ) : entries.length === 0 ? (
          <p className="history-empty">{t("No transactions recorded yet.")}</p>
        ) : (
          <ul className="history-list" data-testid="history-entries-list">
            {entries.map((entry) => (
              <li
                key={entry.seq}
                className={
                  previewSeq === entry.seq ? "history-item history-item-active" : "history-item"
                }
              >
                <button
                  type="button"
                  className="history-item-btn"
                  data-testid={`history-entry-${entry.seq}`}
                  onClick={() => seekSeq(entry.seq)}
                >
                  <span className="history-item-title">
                    #{entry.seq} {translateEngineText(entry.label)}
                  </span>
                  <span className="history-item-meta">
                    {t("{author} (r{replica}) · {time}", {
                      author: personText(entry.authorName),
                      replica: entry.replica,
                      time: formatTimestamp(entry.createdAt),
                    })}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        )}
        {canLoadOlder && (
          <button
            type="button"
            className="toolbar-btn"
            data-testid="history-load-older-btn"
            disabled={busy || loading}
            onClick={() => void handleLoadOlder()}
          >
            {t("Load older revisions")}
          </button>
        )}
      </div>
    </aside>
  );
}
