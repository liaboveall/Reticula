import type { SheetInfo } from "@reticula/protocol";
import { Suspense, useRef, useState } from "react";
import type { CollabWorkbookTarget } from "../collab/CollabShell.tsx";
import type { EditorHandle } from "../commands/handle.ts";
import { TITLES } from "../commands/titles.ts";
import { fmtInt, t } from "../i18n/index.ts";
import { ActionButton } from "../ui/ActionButton.tsx";
import { engineText, hint, lazyPart, withKeys } from "./load-later.ts";
import { keepFocus } from "./ZoomControl.tsx";

const LazyViewMenu = lazyPart(({ ViewMenu }) => ViewMenu);

export interface TitleBarProps {
  readonly handle: EditorHandle;
  readonly title: string;
  /** The sheet in view, or `null` while there is none to work on. */
  readonly sheet: SheetInfo | null;
  /** The user's role in the cloud workbook that is open, or `null` for a local one. */
  readonly collabRole: CollabWorkbookTarget["role"] | null;
  readonly readOnly: boolean;
  readonly canDeleteRows: boolean;
  readonly storageUsedMb: string;
  readonly aiOpen: boolean;
  readonly historyOpen: boolean;
  readonly onNavigate: (path: string) => void;
  readonly onCommand: (id: string) => void;
}

function storagePlace(collabRole: TitleBarProps["collabRole"]): string {
  if (collabRole === null) {
    return t("Local browser storage only");
  }
  return collabRole === "viewer" ? t("Read-only cloud view") : t("Cloud replica cached in OPFS");
}

export function TitleBar({
  handle,
  title,
  sheet,
  collabRole,
  readOnly,
  canDeleteRows,
  storageUsedMb,
  aiOpen,
  historyOpen,
  onNavigate,
  onCommand,
}: TitleBarProps) {
  const viewRef = useRef<HTMLButtonElement>(null);
  const [viewOpen, setViewOpen] = useState(false);
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
        aria-haspopup="dialog"
        onClick={() => onCommand("workbooks")}
        title={hint("workbooks")}
      >
        {t("Workbooks")}
      </button>
      <button
        type="button"
        className="toolbar-btn"
        data-testid="cloud-menu-btn"
        aria-haspopup="dialog"
        onClick={() => onCommand("cloud")}
        title={hint("cloud")}
      >
        {t("Cloud")}
      </button>
      <button
        type="button"
        className="toolbar-btn"
        data-testid="ai-panel-btn"
        aria-expanded={aiOpen}
        onClick={() => onCommand("ai")}
        title={hint("ai")}
      >
        {t("AI")}
      </button>
      {collabRole !== null && (
        <button
          type="button"
          className="toolbar-btn"
          onMouseDown={keepFocus}
          data-testid="history-btn"
          aria-expanded={historyOpen}
          onClick={() => onCommand("history")}
          title={hint("history")}
        >
          {t("History")}
        </button>
      )}
      <button
        ref={viewRef}
        type="button"
        className="toolbar-btn"
        data-testid="view-menu-btn"
        aria-haspopup="menu"
        aria-expanded={viewOpen}
        onMouseDown={keepFocus}
        onClick={() => setViewOpen((open) => !open)}
        onKeyDown={(event) => {
          if (event.key === "ArrowDown") {
            event.preventDefault();
            setViewOpen(true);
          }
        }}
      >
        {t("View")}
      </button>
      {viewOpen && (
        <Suspense fallback={null}>
          <LazyViewMenu handle={handle} anchor={viewRef} onClose={() => setViewOpen(false)} />
        </Suspense>
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
        <ActionButton
          inactive={!sheet}
          className="storage-export-link"
          data-testid="export-rtcl-btn"
          onClick={() => onCommand("exportRtcl")}
        >
          {t("Export .rtcl")}
        </ActionButton>
      </span>
      <div className="toolbar">
        <ActionButton
          inactive={!sheet?.canUndo || readOnly}
          className="toolbar-btn"
          onMouseDown={keepFocus}
          data-testid="undo-btn"
          onClick={() => onCommand("undo")}
          title={withKeys(
            sheet?.undoLabel
              ? t("Undo {action}", { action: engineText(sheet.undoLabel) })
              : TITLES.undo(),
            "undo",
          )}
        >
          ↶ {TITLES.undo()}
        </ActionButton>
        <ActionButton
          inactive={!sheet?.canRedo || readOnly}
          className="toolbar-btn"
          onMouseDown={keepFocus}
          data-testid="redo-btn"
          onClick={() => onCommand("redo")}
          title={withKeys(
            sheet?.redoLabel
              ? t("Redo {action}", { action: engineText(sheet.redoLabel) })
              : TITLES.redo(),
            "redo",
          )}
        >
          ↷ {TITLES.redo()}
        </ActionButton>
        <ActionButton
          inactive={!sheet || readOnly}
          className="toolbar-btn"
          onMouseDown={keepFocus}
          data-testid="insert-row-above"
          onClick={() => onCommand("insertRowAbove")}
        >
          + {TITLES.insertRowAbove()}
        </ActionButton>
        <ActionButton
          inactive={!sheet || !canDeleteRows}
          className="toolbar-btn"
          onMouseDown={keepFocus}
          data-testid="delete-rows"
          onClick={() => onCommand("deleteRows")}
        >
          {TITLES.deleteRows()}
        </ActionButton>
      </div>
    </header>
  );
}
