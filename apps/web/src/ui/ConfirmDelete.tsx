import { useId, useRef } from "react";
import { t } from "../i18n/index.ts";
import { Dialog } from "./Dialog.tsx";

interface ConfirmDeleteProps {
  /** The name of what is deleted, as its list shows it. */
  readonly title: string;
  /** What deleting it does, which cannot be undone. */
  readonly consequence: string;
  readonly onConfirm: () => void;
  readonly onCancel: () => void;
  /** Takes the focus when the button that opened it is gone. */
  readonly fallbackFocus?: () => void;
}

/** Asks before something is deleted that cannot be had back. Keeping it is the default. */
export function ConfirmDelete({
  title,
  consequence,
  onConfirm,
  onCancel,
  fallbackFocus,
}: ConfirmDeleteProps) {
  const id = useId();
  const cancelRef = useRef<HTMLButtonElement>(null);
  return (
    <Dialog
      labelledBy={`${id}-heading`}
      describedBy={`${id}-text`}
      initialFocus={cancelRef}
      fallbackFocus={fallbackFocus}
      onClose={onCancel}
      className="dialog confirm-delete"
      testId="confirm-delete"
    >
      <div className="dialog-header">
        <h2 id={`${id}-heading`}>{t('Delete "{title}"?', { title })}</h2>
      </div>
      <p id={`${id}-text`} className="confirm-delete-text">
        {consequence}
      </p>
      <div className="dialog-footer">
        <button ref={cancelRef} type="button" className="toolbar-btn" onClick={onCancel}>
          {t("Cancel")}
        </button>
        <button
          type="button"
          className="toolbar-btn danger-btn"
          data-testid="confirm-delete-btn"
          onClick={onConfirm}
        >
          {t("Delete")}
        </button>
      </div>
    </Dialog>
  );
}
