import type { SheetInfo, SheetTabInfo } from "@reticula/protocol";
import { t } from "../i18n/index.ts";
import type { ClipboardAction, ContextMenuState } from "./types.ts";

export interface ContextMenuProps {
  readonly menu: ContextMenuState;
  readonly sheet: SheetInfo | null;
  readonly readOnly: boolean;
  readonly canDeleteRows: boolean;
  readonly canDeleteCols: boolean;
  readonly onClose: () => void;
  readonly onClipboard: (action: ClipboardAction) => void;
  readonly onClearContents: () => void;
  readonly onInsertRow: (below: boolean) => void;
  readonly onDeleteRows: () => void;
  readonly onInsertCol: (right: boolean) => void;
  readonly onDeleteCols: () => void;
  readonly onRenameSheet: (tab: SheetTabInfo) => void;
  /** Puts a sheet after another one, or before all of them. */
  readonly onMoveSheet: (tab: SheetTabInfo, afterSheet: number | undefined) => void;
  readonly onDeleteSheet: (tab: SheetTabInfo) => void;
}

export function ContextMenu(props: ContextMenuProps) {
  const { menu } = props;
  const { clientX, clientY } = menu.kind === "grid" ? menu.target : menu;
  return (
    <div
      className="context-menu"
      role="menu"
      data-testid="context-menu"
      style={{
        left: `${Math.min(window.innerWidth - 208, Math.max(8, clientX))}px`,
        top: `${Math.min(window.innerHeight - 260, Math.max(8, clientY))}px`,
      }}
    >
      {menu.kind === "grid" ? <GridMenu {...props} /> : <TabMenu {...props} tab={menu.tab} />}
    </div>
  );
}

function GridMenu({
  readOnly,
  canDeleteRows,
  canDeleteCols,
  onClose,
  onClipboard,
  onClearContents,
  onInsertRow,
  onDeleteRows,
  onInsertCol,
  onDeleteCols,
}: ContextMenuProps) {
  return (
    <>
      <button
        type="button"
        role="menuitem"
        className="ctx-item"
        data-testid="ctx-cut"
        disabled={readOnly}
        onClick={() => onClipboard("cut")}
      >
        {t("Cut")} <kbd>Ctrl+X</kbd>
      </button>
      <button
        type="button"
        role="menuitem"
        className="ctx-item"
        data-testid="ctx-copy"
        onClick={() => onClipboard("copy")}
      >
        {t("Copy")} <kbd>Ctrl+C</kbd>
      </button>
      <button
        type="button"
        role="menuitem"
        className="ctx-item"
        data-testid="ctx-paste"
        disabled={readOnly}
        onClick={() => onClipboard("paste")}
      >
        {t("Paste")} <kbd>Ctrl+V</kbd>
      </button>
      <button
        type="button"
        role="menuitem"
        className="ctx-item"
        data-testid="ctx-clear"
        disabled={readOnly}
        onClick={() => {
          onClose();
          onClearContents();
        }}
      >
        {t("Clear Contents")} <kbd>Del</kbd>
      </button>
      <div className="ctx-sep" />
      <button
        type="button"
        role="menuitem"
        className="ctx-item"
        data-testid="ctx-insert-row-above"
        disabled={readOnly}
        onClick={() => {
          onClose();
          onInsertRow(false);
        }}
      >
        {t("Insert Row Above")}
      </button>
      <button
        type="button"
        role="menuitem"
        className="ctx-item"
        data-testid="ctx-insert-row-below"
        disabled={readOnly}
        onClick={() => {
          onClose();
          onInsertRow(true);
        }}
      >
        {t("Insert Row Below")}
      </button>
      <button
        type="button"
        role="menuitem"
        className="ctx-item"
        data-testid="ctx-delete-rows"
        disabled={!canDeleteRows}
        onClick={() => {
          onClose();
          onDeleteRows();
        }}
      >
        {t("Delete Selected Rows")}
      </button>
      <div className="ctx-sep" />
      <button
        type="button"
        role="menuitem"
        className="ctx-item"
        data-testid="ctx-insert-col-left"
        disabled={readOnly}
        onClick={() => {
          onClose();
          onInsertCol(false);
        }}
      >
        {t("Insert Column Left")}
      </button>
      <button
        type="button"
        role="menuitem"
        className="ctx-item"
        data-testid="ctx-insert-col-right"
        disabled={readOnly}
        onClick={() => {
          onClose();
          onInsertCol(true);
        }}
      >
        {t("Insert Column Right")}
      </button>
      <button
        type="button"
        role="menuitem"
        className="ctx-item"
        data-testid="ctx-delete-cols"
        disabled={!canDeleteCols}
        onClick={() => {
          onClose();
          onDeleteCols();
        }}
      >
        {t("Delete Selected Columns")}
      </button>
    </>
  );
}

interface TabMenuProps extends ContextMenuProps {
  readonly tab: SheetTabInfo;
}

function TabMenu({
  sheet,
  readOnly,
  tab,
  onClose,
  onRenameSheet,
  onMoveSheet,
  onDeleteSheet,
}: TabMenuProps) {
  const sheets = sheet?.sheets ?? [];
  const idx = sheets.findIndex((s) => s.id === tab.id);
  const canLeft = !readOnly && idx > 0;
  const canRight = !readOnly && idx >= 0 && idx < sheets.length - 1;
  return (
    <>
      <button
        type="button"
        role="menuitem"
        className="ctx-item"
        data-testid="ctx-rename-sheet"
        disabled={readOnly}
        onClick={() => {
          onClose();
          onRenameSheet(tab);
        }}
      >
        {t("Rename Sheet…")}
      </button>
      {sheet && (
        <>
          <button
            type="button"
            role="menuitem"
            className="ctx-item"
            data-testid="ctx-move-sheet-left"
            disabled={!canLeft}
            onClick={() => {
              onClose();
              onMoveSheet(tab, idx >= 2 ? sheets[idx - 2]?.id : undefined);
            }}
          >
            {t("Move Left")}
          </button>
          <button
            type="button"
            role="menuitem"
            className="ctx-item"
            data-testid="ctx-move-sheet-right"
            disabled={!canRight}
            onClick={() => {
              onClose();
              const next = sheets[idx + 1];
              if (next) {
                onMoveSheet(tab, next.id);
              }
            }}
          >
            {t("Move Right")}
          </button>
          <div className="ctx-sep" />
          <button
            type="button"
            role="menuitem"
            className="ctx-item danger"
            data-testid="ctx-delete-sheet"
            disabled={readOnly || sheets.length <= 1}
            onClick={() => {
              onClose();
              onDeleteSheet(tab);
            }}
          >
            {t("Delete Sheet")}
          </button>
        </>
      )}
    </>
  );
}
