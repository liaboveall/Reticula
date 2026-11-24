import { t } from "../i18n/index.ts";
import type { LockOverlayState } from "./types.ts";

export interface LockOverlayProps {
  readonly lock: LockOverlayState;
  readonly onTakeOver: (docId: string) => void;
  readonly onNewWorkbook: () => void;
}

function heading(lock: LockOverlayState): string {
  switch (lock.kind) {
    case "blocked":
      return t("This local workbook is open in another tab");
    case "takingOver":
      return t("Moving workbook to this tab…");
    case "yielded":
      return t("Workbook moved to another tab");
  }
}

function explanation(lock: LockOverlayState): string {
  switch (lock.kind) {
    case "blocked":
      return t(
        "“{title}” is exclusively locked by another browser tab to keep its OPFS edit log consistent.",
        { title: lock.title },
      );
    case "takingOver":
      return lock.waitingForFile
        ? t(
            "Waiting for the other tab to release the file — if the other tab is unresponsive, please close it.",
          )
        : t("Asking the other tab to flush and release the workbook…");
    case "yielded":
      return lock.evicted
        ? t("Another tab took over this workbook because this tab did not release it in time.")
        : t("This tab flushed its changes and handed the workbook over to another tab.");
  }
}

/** Covers the grid while another tab has the workbook. */
export function LockOverlay({ lock, onTakeOver, onNewWorkbook }: LockOverlayProps) {
  return (
    <div className="lock-overlay" role="dialog" aria-modal="true" data-testid="lock-overlay">
      <div className="lock-card">
        <h2>{heading(lock)}</h2>
        <p data-testid="lock-overlay-message">{explanation(lock)}</p>
        {lock.kind !== "takingOver" && (
          <div className="lock-actions">
            <button
              type="button"
              className="toolbar-btn primary-btn"
              data-testid="take-over-btn"
              onClick={() => onTakeOver(lock.docId)}
            >
              {t("Open here (Take over)")}
            </button>
            <button
              type="button"
              className="toolbar-btn"
              data-testid="new-workbook-from-lock-btn"
              onClick={onNewWorkbook}
            >
              {t("New local workbook")}
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
