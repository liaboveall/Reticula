import type { SheetInfo } from "@reticula/protocol";
import { useState } from "react";
import { translateEngineText } from "../i18n/engine-text.ts";
import { t } from "../i18n/index.ts";
import { tx } from "../i18n/react.ts";

export interface QuotaBannerProps {
  readonly message: string | null;
  readonly onExportRtcl: () => void;
  readonly onOpenWorkbooks: () => void;
  readonly onDismiss: () => void;
}

/** Says that the browser is running out of storage, and what an import left behind. */
export function QuotaBanner({
  message,
  onExportRtcl,
  onOpenWorkbooks,
  onDismiss,
}: QuotaBannerProps) {
  if (!message) {
    return null;
  }
  return (
    <div className="rejected-banner" role="alert" data-testid="quota-banner">
      <span>{translateEngineText(message)}</span>
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

export interface UpdateBannerProps {
  readonly ready: boolean;
  /** Reloads into the new version; rejects if that did not work out. */
  readonly onApply: () => Promise<void> | undefined;
  readonly onDismiss: () => void;
}

export function UpdateBanner({ ready, onApply, onDismiss }: UpdateBannerProps) {
  const [updating, setUpdating] = useState(false);
  if (!ready) {
    return null;
  }
  return (
    <div className="sw-update-banner" role="status" data-testid="sw-update-banner">
      <span>
        {t(
          "A new version of Reticula is ready. Confirming flushes all open tabs and reloads safely.",
        )}
      </span>
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

export interface FrozenRecalcBannerProps {
  readonly sheet: SheetInfo | null;
  readonly onResume: () => void;
}

export function FrozenRecalcBanner({ sheet, onResume }: FrozenRecalcBannerProps) {
  if (!sheet?.recalcFrozen) {
    return null;
  }
  return (
    <div className="frozen-recalc-banner" role="status" data-testid="recalc-frozen-banner">
      <span>
        {t(
          "Safe Mode opened this workbook with automatic recalculation paused so you can inspect or edit formulas safely.",
        )}
      </span>
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

export interface DemoTipProps {
  readonly sheet: SheetInfo | null;
  readonly onJump: () => void;
}

/** What to try first, above the first sheet of a generated sample. */
export function DemoTip({ sheet, onJump }: DemoTipProps) {
  const [open, setOpen] = useState(true);
  if (!open || !sheet || sheet.seed === 0 || sheet.sheet !== 0) {
    return null;
  }
  return (
    <div className="demo-tip" data-testid="demo-tip">
      <span>
        {tx(
          "Try live recalculation: edit parameter cell <0>V1</0> (e.g. type <1>5%</1> and press Enter) to recalculate <2>100,001</2> dependent formulas.",
          { 0: <code />, 1: <code />, 2: <strong /> },
        )}
      </span>
      <button type="button" className="tip-action" data-testid="jump-v1" onClick={onJump}>
        {t("Jump to V1")}
      </button>
      <button
        type="button"
        className="tip-close"
        aria-label={t("Dismiss tip")}
        onClick={() => setOpen(false)}
      >
        ×
      </button>
    </div>
  );
}

export interface AiDraftBannerProps {
  readonly active: boolean;
  readonly onExit: () => void;
}

export function AiDraftBanner({ active, onExit }: AiDraftBannerProps) {
  if (!active) {
    return null;
  }
  return (
    <div className="history-preview-banner" data-testid="ai-draft-banner" role="status">
      <span>{t("Viewing AI Draft · Main untouched")}</span>
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

export interface HistoryPreviewBannerProps {
  /** The revision in view, or `null` outside the history. */
  readonly seq: string | null;
  readonly onReturn: () => void;
}

export function HistoryPreviewBanner({ seq, onReturn }: HistoryPreviewBannerProps) {
  if (seq === null) {
    return null;
  }
  return (
    <div className="history-preview-banner" data-testid="history-preview-banner" role="status">
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

export interface RejectedBannerProps {
  readonly reason: string | null;
  readonly onDismiss: () => void;
}

/** Says why the engine refused the last command. */
export function RejectedBanner({ reason, onDismiss }: RejectedBannerProps) {
  if (!reason) {
    return null;
  }
  return (
    <div className="rejected-banner" role="alert" data-testid="rejected-banner">
      <span>{translateEngineText(reason)}</span>
      <button
        type="button"
        className="tip-close"
        aria-label={t("Dismiss error")}
        onClick={onDismiss}
      >
        ×
      </button>
    </div>
  );
}
