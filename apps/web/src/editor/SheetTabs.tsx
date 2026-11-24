import type { SheetInfo, SheetTabInfo } from "@reticula/protocol";
import { t } from "../i18n/index.ts";
import type { SheetRename } from "./types.ts";

export interface SheetTabsProps {
  readonly sheet: SheetInfo;
  readonly readOnly: boolean;
  readonly renaming: SheetRename | null;
  readonly onRenaming: (next: SheetRename | null) => void;
  readonly onRename: (tab: SheetTabInfo, name: string) => void;
  /** Gives the focus back to the grid. */
  readonly onLeave: () => void;
  readonly onSwitch: (id: number) => void;
  readonly onMenu: (tab: SheetTabInfo, clientX: number, clientY: number) => void;
  readonly onAdd: () => void;
}

export function SheetTabs({
  sheet,
  readOnly,
  renaming,
  onRenaming,
  onRename,
  onLeave,
  onSwitch,
  onMenu,
  onAdd,
}: SheetTabsProps) {
  const commit = (tab: SheetTabInfo, draft: string) => {
    const trimmed = draft.trim();
    onRenaming(null);
    if (trimmed && trimmed !== tab.name) {
      onRename(tab, trimmed);
    }
  };
  return (
    <div className="sheetbar" role="tablist" aria-label={t("Worksheets")} data-testid="sheet-tabs">
      {sheet.sheets.map((tab) => {
        const isActive = tab.id === sheet.sheet;
        if (renaming?.id === tab.id) {
          return (
            <input
              key={tab.id}
              type="text"
              className="sheet-rename-input"
              data-testid="rename-sheet-input"
              value={renaming.draft}
              maxLength={31}
              // biome-ignore lint/a11y/noAutofocus: inline tab rename requires immediate focus
              autoFocus
              onChange={(e) => onRenaming({ id: tab.id, draft: e.target.value })}
              onBlur={() => commit(tab, renaming.draft)}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  commit(tab, renaming.draft);
                  onLeave();
                } else if (e.key === "Escape") {
                  e.preventDefault();
                  onRenaming(null);
                  onLeave();
                }
              }}
            />
          );
        }
        return (
          <button
            key={tab.id}
            type="button"
            role="tab"
            aria-selected={isActive}
            className={isActive ? "sheet-tab active" : "sheet-tab"}
            data-testid={`sheet-tab-${tab.id}`}
            onClick={() => onSwitch(tab.id)}
            onDoubleClick={() => {
              if (!readOnly) {
                onRenaming({ id: tab.id, draft: tab.name });
              }
            }}
            onContextMenu={(e) => {
              e.preventDefault();
              onMenu(tab, e.clientX, e.clientY);
            }}
          >
            {tab.name}
          </button>
        );
      })}
      <button
        type="button"
        className="sheet-add-btn"
        data-testid="add-sheet-btn"
        disabled={readOnly}
        title={t("Add worksheet")}
        aria-label={t("Add worksheet")}
        onClick={onAdd}
      >
        +
      </button>
    </div>
  );
}
