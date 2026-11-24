import type { SheetInfo, SheetTabInfo } from "@reticula/protocol";
import { type KeyboardEvent, Suspense, useEffect, useLayoutEffect, useRef } from "react";
import { t } from "../i18n/index.ts";
import { swallowContextMenu } from "../keymap.ts";
import { ActionButton } from "../ui/ActionButton.tsx";
import { lazyPart } from "./load-later.ts";

const LazySheetRename = lazyPart(({ SheetRename }) => SheetRename);

export interface SheetTabsProps {
  readonly sheet: SheetInfo;
  readonly readOnly: boolean;
  /** The sheet that is being renamed. */
  readonly renaming: number | null;
  readonly onRename: (tab: SheetTabInfo, name: string | null, byKey: boolean) => void;
  /** Shows a sheet, and gives the focus to the grid if it was not gone to with the keys. */
  readonly onSwitch: (id: number, byKey: boolean) => void;
  readonly onMenu: (tab: SheetTabInfo, clientX: number, clientY: number) => void;
  readonly onCommand: (id: string, sheet: number) => void;
}

export function SheetTabs({
  sheet,
  readOnly,
  renaming,
  onRename,
  onSwitch,
  onMenu,
  onCommand,
}: SheetTabsProps) {
  const listRef = useRef<HTMLDivElement>(null);
  const { sheets } = sheet;
  const renamed = sheets.find(({ id }) => id === renaming);

  // The focus does not go with the tab that had it. React says nothing of what
  // happens to the focus while it changes the page.
  const hadFocus = useRef(false);
  useEffect(() => {
    const list = listRef.current;
    const onFocusIn = () => {
      hadFocus.current = true;
    };
    const onFocusOut = ({ target }: FocusEvent) => {
      // Some browsers say so of an element that is taken away.
      queueMicrotask(() => {
        hadFocus.current &&=
          list?.contains(document.activeElement) === true ||
          (target instanceof Node && !target.isConnected);
      });
    };
    list?.addEventListener("focusin", onFocusIn);
    list?.addEventListener("focusout", onFocusOut);
    return () => {
      list?.removeEventListener("focusin", onFocusIn);
      list?.removeEventListener("focusout", onFocusOut);
    };
  }, []);
  useLayoutEffect(() => {
    if (hadFocus.current && document.activeElement === document.body) {
      listRef.current?.querySelector<HTMLElement>('[aria-selected="true"]')?.focus();
    }
  });

  const focusTab = (index: number) => {
    const tab = sheets[index];
    if (tab) {
      listRef.current?.querySelectorAll<HTMLElement>('[role="tab"]')[index]?.focus();
      onSwitch(tab.id, true);
    }
  };

  const onKeyDown = (event: KeyboardEvent<HTMLElement>, tab: SheetTabInfo, index: number) => {
    const { key } = event;
    if (event.ctrlKey || event.metaKey || event.altKey) {
      return;
    }
    if ((key === "F10" && event.shiftKey) || key === "ContextMenu") {
      swallowContextMenu();
      const { left, top } = event.currentTarget.getBoundingClientRect();
      onMenu(tab, left, top);
    } else if (key === "F2") {
      onCommand("renameSheet", tab.id);
    } else if (key === "Delete") {
      onCommand("deleteSheet", tab.id);
    } else if (key === "ArrowLeft" || key === "ArrowRight") {
      const { length } = sheets;
      focusTab((index + (key === "ArrowLeft" ? -1 : 1) + length) % length);
    } else if (key === "Home" || key === "End") {
      focusTab(key === "Home" ? 0 : sheets.length - 1);
    } else {
      return;
    }
    event.preventDefault();
  };

  return (
    <div className="sheetbar" data-testid="sheet-tabs">
      <div ref={listRef} className="sheet-list" role="tablist" aria-label={t("Worksheets")}>
        {sheets.map((tab, index) => {
          const isActive = tab.id === sheet.sheet;
          return (
            <button
              key={tab.id}
              type="button"
              role="tab"
              aria-selected={isActive}
              tabIndex={isActive ? 0 : -1}
              className={isActive ? "sheet-tab active" : "sheet-tab"}
              data-testid={`sheet-tab-${tab.id}`}
              onClick={() => onSwitch(tab.id, false)}
              onDoubleClick={() => onCommand("renameSheet", tab.id)}
              onContextMenu={(event) => {
                event.preventDefault();
                onMenu(tab, event.clientX, event.clientY);
              }}
              onKeyDown={(event) => onKeyDown(event, tab, index)}
            >
              {tab.name}
            </button>
          );
        })}
      </div>
      {renamed && (
        <Suspense fallback={null}>
          <LazySheetRename
            key={renamed.id}
            tab={renamed}
            list={listRef}
            onDone={(name, byKey) => onRename(renamed, name, byKey)}
          />
        </Suspense>
      )}
      <ActionButton
        inactive={readOnly}
        className="sheet-add-btn"
        data-testid="add-sheet-btn"
        title={t("Add worksheet")}
        aria-label={t("Add worksheet")}
        onClick={() => onCommand("addSheet", sheet.sheet)}
      >
        +
      </ActionButton>
    </div>
  );
}
