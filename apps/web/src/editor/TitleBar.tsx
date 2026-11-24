import type { SheetInfo } from "@reticula/protocol";
import type { CollabWorkbookTarget } from "../collab/CollabShell.tsx";
import { translateEngineText } from "../i18n/engine-text.ts";
import { fmtInt, t } from "../i18n/index.ts";

export interface TitleBarProps {
  readonly title: string;
  /** The sheet in view, or `null` while there is none to work on. */
  readonly sheet: SheetInfo | null;
  /** The user's role in the cloud workbook that is open, or `null` for a local one. */
  readonly collabRole: CollabWorkbookTarget["role"] | null;
  readonly readOnly: boolean;
  readonly canDeleteRows: boolean;
  readonly storageUsedMb: string;
  readonly onNavigate: (path: string) => void;
  readonly onOpenWorkbooks: () => void;
  readonly onOpenCloud: () => void;
  readonly onToggleAi: () => void;
  readonly onToggleHistory: () => void;
  readonly onExportRtcl: () => void;
  readonly onUndo: () => void;
  readonly onRedo: () => void;
  readonly onInsertRowAbove: () => void;
  readonly onDeleteRows: () => void;
}

function storagePlace(collabRole: TitleBarProps["collabRole"]): string {
  if (collabRole === null) {
    return t("Local browser storage only");
  }
  return collabRole === "viewer" ? t("Read-only cloud view") : t("Cloud replica cached in OPFS");
}

export function TitleBar({
  title,
  sheet,
  collabRole,
  readOnly,
  canDeleteRows,
  storageUsedMb,
  onNavigate,
  onOpenWorkbooks,
  onOpenCloud,
  onToggleAi,
  onToggleHistory,
  onExportRtcl,
  onUndo,
  onRedo,
  onInsertRowAbove,
  onDeleteRows,
}: TitleBarProps) {
  return (
    <header className="titlebar">
      <a
        href="/"
        className="brand-link"
        onClick={(event) => {
          event.preventDefault();
          onNavigate("/");
        }}
      >
        <h1>Reticula</h1>
      </a>
      <button
        type="button"
        className="toolbar-btn"
        data-testid="workbooks-btn"
        onClick={onOpenWorkbooks}
        title={t("Manage local browser workbooks, import or export .rtcl / .csv")}
      >
        {t("Workbooks")}
      </button>
      <button
        type="button"
        className="toolbar-btn"
        data-testid="cloud-menu-btn"
        onClick={onOpenCloud}
        title={t("Sign in, create, or share collaborative cloud workbooks")}
      >
        {t("Cloud")}
      </button>
      <button
        type="button"
        className="toolbar-btn"
        data-testid="ai-panel-btn"
        onClick={onToggleAi}
        title={t("Open AI Analyst draft assistant and recorded demos")}
      >
        {t("AI")}
      </button>
      {collabRole !== null && (
        <button
          type="button"
          className="toolbar-btn"
          data-testid="history-btn"
          onClick={onToggleHistory}
          title={t("Browse revision history and named versions")}
        >
          {t("History")}
        </button>
      )}
      {sheet && (
        <span className="document" data-testid="document">
          {t("{title} · {rows} rows × {cols} columns · {formulas} formulas", {
            title,
            rows: fmtInt(sheet.rows),
            cols: sheet.cols,
            formulas: fmtInt(sheet.formulaCount),
          })}
        </span>
      )}
      <span className="titlebar-spacer" />
      <span className="storage-badge" data-testid="storage-badge">
        {storagePlace(collabRole)} · {t("{size} MB used", { size: storageUsedMb })} ·{" "}
        <button
          type="button"
          className="storage-export-link"
          data-testid="export-rtcl-btn"
          disabled={!sheet}
          onClick={onExportRtcl}
        >
          {t("Export .rtcl")}
        </button>
      </span>
      <div className="toolbar">
        <button
          type="button"
          className="toolbar-btn"
          data-testid="undo-btn"
          disabled={!sheet?.canUndo || readOnly}
          onClick={onUndo}
          title={
            sheet?.undoLabel
              ? t("Undo {action} (Ctrl+Z)", { action: translateEngineText(sheet.undoLabel) })
              : t("Undo (Ctrl+Z)")
          }
        >
          ↶ {t("Undo")}
        </button>
        <button
          type="button"
          className="toolbar-btn"
          data-testid="redo-btn"
          disabled={!sheet?.canRedo || readOnly}
          onClick={onRedo}
          title={
            sheet?.redoLabel
              ? t("Redo {action} (Ctrl+Y)", { action: translateEngineText(sheet.redoLabel) })
              : t("Redo (Ctrl+Y)")
          }
        >
          ↷ {t("Redo")}
        </button>
        <button
          type="button"
          className="toolbar-btn"
          data-testid="insert-row-above"
          disabled={!sheet || readOnly}
          onClick={onInsertRowAbove}
          title={t("Insert 1 row above the selected row (at row 1 if nothing is selected)")}
        >
          + {t("Insert Row Above")}
        </button>
        <button
          type="button"
          className="toolbar-btn"
          data-testid="delete-rows"
          disabled={!sheet || !canDeleteRows}
          onClick={onDeleteRows}
          title={t("Delete all rows spanned by the current selection")}
        >
          {t("Delete Selected Rows")}
        </button>
      </div>
    </header>
  );
}
