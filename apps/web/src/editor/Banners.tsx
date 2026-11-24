import type { SheetInfo } from "@reticula/protocol";
import { useEffect, useState } from "react";
import { translateEngineText } from "../i18n/engine-text.ts";
import { t } from "../i18n/index.ts";
import { announce } from "../ui/announcer.ts";

/** Has a banner read out that has just appeared: a live region that comes with its text is not. */
function useAnnounced(text: string, politeness: "polite" | "assertive" = "polite"): void {
  useEffect(() => announce(text, { politeness }), [text, politeness]);
}

export interface BannersProps {
  readonly sheet: SheetInfo | null;
  /** Why the engine refused the last command. */
  readonly rejected: string | null;
  readonly onDismissRejected: () => void;
  /** That the browser is running out of storage, and what an import left behind. */
  readonly storage: string | null;
  readonly onDismissStorage: () => void;
  readonly onExportRtcl: () => void;
  readonly onOpenWorkbooks: () => void;
  readonly updateReady: boolean;
  /** Reloads into the new version; rejects if that did not work out. */
  readonly onUpdate: () => Promise<void> | undefined;
  readonly onDismissUpdate: () => void;
  readonly onResumeRecalc: () => void;
  readonly isDraft: boolean;
  readonly onExitDraft: () => void;
  /** The revision in view, or `null` outside the history. */
  readonly historySeq: string | null;
  readonly onReturnLive: () => void;
}

/** What there is to know about the workbook that is not always the case. */
export default function Banners(props: BannersProps) {
  return (
    <>
      {props.storage && (
        <StorageBanner
          message={props.storage}
          onExportRtcl={props.onExportRtcl}
          onOpenWorkbooks={props.onOpenWorkbooks}
          onDismiss={props.onDismissStorage}
        />
      )}
      {props.updateReady && (
        <UpdateBanner onApply={props.onUpdate} onDismiss={props.onDismissUpdate} />
      )}
      {props.sheet?.recalcFrozen && <FrozenRecalcBanner onResume={props.onResumeRecalc} />}
      {props.isDraft && <DraftBanner onExit={props.onExitDraft} />}
      {props.historySeq !== null && (
        <HistoryBanner seq={props.historySeq} onReturn={props.onReturnLive} />
      )}
      {props.rejected && (
        <div className="rejected-banner" data-testid="rejected-banner">
          <span>{translateEngineText(props.rejected)}</span>
          <button
            type="button"
            className="tip-close"
            aria-label={t("Dismiss error")}
            onClick={props.onDismissRejected}
          >
            ×
          </button>
        </div>
      )}
    </>
  );
}

function StorageBanner({
  message,
  onExportRtcl,
  onOpenWorkbooks,
  onDismiss,
}: {
  readonly message: string;
  readonly onExportRtcl: () => void;
  readonly onOpenWorkbooks: () => void;
  readonly onDismiss: () => void;
}) {
  const text = translateEngineText(message);
  useAnnounced(text, "assertive");
  return (
    <div className="rejected-banner" data-testid="quota-banner">
      <span>{text}</span>
      <div className="banner-actions">
        <button type="button" className="tip-action" onClick={onExportRtcl}>
          {t("Export .rtcl")}
        </button>
        <button type="button" className="tip-action" onClick={onOpenWorkbooks}>
          {t("Manage Workbooks")}
        </button>
        <button
          type="button"
          className="tip-close"
          aria-label={t("Dismiss storage warning")}
          onClick={onDismiss}
        >
          ×
        </button>
      </div>
    </div>
  );
}

function UpdateBanner({
  onApply,
  onDismiss,
}: {
  readonly onApply: () => Promise<void> | undefined;
  readonly onDismiss: () => void;
}) {
  const [updating, setUpdating] = useState(false);
  const text = t(
    "A new version of Reticula is ready. Confirming flushes all open tabs and reloads safely.",
  );
  useAnnounced(text);
  return (
    <div className="sw-update-banner" data-testid="sw-update-banner">
      <span>{text}</span>
      <div className="quota-banner-actions">
        <button
          type="button"
          className="tip-action"
          data-testid="sw-update-btn"
          disabled={updating}
          onClick={() => {
            setUpdating(true);
            void onApply()?.catch(() => setUpdating(false));
          }}
        >
          {updating ? t("Updating…") : t("Update now")}
        </button>
        <button
          type="button"
          className="tip-close"
          aria-label={t("Dismiss update notification")}
          onClick={onDismiss}
        >
          ×
        </button>
      </div>
    </div>
  );
}

function FrozenRecalcBanner({ onResume }: { readonly onResume: () => void }) {
  const text = t(
    "Safe Mode opened this workbook with automatic recalculation paused so you can inspect or edit formulas safely.",
  );
  useAnnounced(text);
  return (
    <div className="frozen-recalc-banner" data-testid="recalc-frozen-banner">
      <span>{text}</span>
      <button
        type="button"
        className="tip-action"
        data-testid="resume-recalc-btn"
        onClick={onResume}
      >
        {t("Resume Recalculation")}
      </button>
    </div>
  );
}

function DraftBanner({ onExit }: { readonly onExit: () => void }) {
  const text = t("Viewing AI Draft · Main untouched");
  useAnnounced(text);
  return (
    <div className="history-preview-banner" data-testid="ai-draft-banner">
      <span>{text}</span>
      <button
        type="button"
        className="toolbar-btn primary-btn"
        data-testid="ai-banner-exit-btn"
        onClick={onExit}
      >
        {t("Back to Main")}
      </button>
    </div>
  );
}

function HistoryBanner({ seq, onReturn }: { readonly seq: string; readonly onReturn: () => void }) {
  return (
    <div className="history-preview-banner" data-testid="history-preview-banner">
      <span>{t("Previewing history #{seq} (read-only)", { seq })}</span>
      <button
        type="button"
        className="toolbar-btn primary-btn"
        data-testid="history-banner-return-live-btn"
        onClick={onReturn}
      >
        {t("Return to Live")}
      </button>
    </div>
  );
}
