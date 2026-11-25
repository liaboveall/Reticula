import type { SheetInfo, SheetTabInfo } from "@reticula/protocol";
import { type KeyboardEvent, Suspense, useEffect, useLayoutEffect, useRef } from "react";
import type { TableAsk } from "../commands/handle.ts";
import { t } from "../i18n/index.ts";
import { swallowContextMenu } from "../keymap.ts";
import type { TableEntry } from "../tables/catalog.ts";
import { ActionButton } from "../ui/ActionButton.tsx";
import { lazyPart } from "./load-later.ts";

const LazySheetRename = lazyPart(({ SheetRename }) => SheetRename);

type Tab = SheetTabInfo | TableEntry;

const isTable = (tab: Tab): tab is TableEntry => typeof tab.id === "string";

export interface SheetTabsProps {
  readonly sheet: SheetInfo;
  /** The data tables, which come after the worksheets. */
  readonly tables: readonly TableEntry[];
  /** The data table in view in place of the worksheet. */
  readonly table: string | null;
  readonly readOnly: boolean;
  /** The sheet that is being renamed. */
  readonly renaming: number | null;
  readonly onRename: (tab: SheetTabInfo, name: string | null, byKey: boolean) => void;
  /** Shows a sheet, and gives the focus to the grid if it was not gone to with the keys. */
  readonly onSwitch: (id: number, byKey: boolean) => void;
  readonly onMenu: (tab: SheetTabInfo, clientX: number, clientY: number) => void;
  readonly onCommand: (id: string, sheet: number) => void;
  readonly onTable: (id: string, byKey: boolean) => void;
  readonly onTables: (ask: TableAsk) => void;
}

export function SheetTabs({
  sheet,
  tables,
  table,
  readOnly,
  renaming,
  onRename,
  onSwitch,
  onMenu,
  onCommand,
  onTable,
  onTables,
}: SheetTabsProps) {
  const listRef = useRef<HTMLDivElement>(null);
  const { sheets } = sheet;
  const tabs: readonly Tab[] = [...sheets, ...tables];
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

  const show = (tab: Tab, byKey: boolean) =>
    isTable(tab) ? onTable(tab.id, byKey) : onSwitch(tab.id, byKey);
  const act = (tab: Tab, kind: "rename" | "delete") =>
    isTable(tab) ? onTables({ kind, id: tab.id }) : onCommand(`${kind}Sheet`, tab.id);
  const openMenu = (tab: Tab, x: number, y: number) =>
    isTable(tab) ? onTables({ kind: "menu", id: tab.id, x, y }) : onMenu(tab, x, y);

  const focusTab = (index: number) => {
    const tab = tabs[index];
    if (tab) {
      listRef.current?.querySelectorAll<HTMLElement>('[role="tab"]')[index]?.focus();
      show(tab, true);
    }
  };

  const onKeyDown = (event: KeyboardEvent<HTMLElement>, tab: Tab, index: number) => {
    const { key } = event;
    if (event.ctrlKey || event.metaKey || event.altKey) {
      return;
    }
    if ((key === "F10" && event.shiftKey) || key === "ContextMenu") {
      swallowContextMenu();
      const { left, top } = event.currentTarget.getBoundingClientRect();
      openMenu(tab, left, top);
    } else if (key === "F2") {
      act(tab, "rename");
    } else if (key === "Delete") {
      act(tab, "delete");
    } else if (key === "ArrowLeft" || key === "ArrowRight") {
      const { length } = tabs;
      focusTab((index + (key === "ArrowLeft" ? -1 : 1) + length) % length);
    } else if (key === "Home" || key === "End") {
      focusTab(key === "Home" ? 0 : tabs.length - 1);
    } else {
      return;
    }
    event.preventDefault();
  };

  return (
    <div className="sheetbar" data-testid="sheet-tabs">
      <div ref={listRef} className="sheet-list" role="tablist" aria-label={t("Worksheets")}>
        {tabs.map((tab, index) => {
          const isActive = tab.id === (table ?? sheet.sheet);
          return (
            <button
              key={tab.id}
              type="button"
              role="tab"
              aria-selected={isActive}
              tabIndex={isActive ? 0 : -1}
              className={`sheet-tab${isActive ? " active" : ""}${isTable(tab) ? " table-tab" : ""}`}
              title={isTable(tab) ? t("Data table") : undefined}
              data-testid={`${isTable(tab) ? "table" : "sheet"}-tab-${tab.id}`}
              onClick={() => show(tab, false)}
              onDoubleClick={() => act(tab, "rename")}
              onContextMenu={(event) => {
                event.preventDefault();
                openMenu(tab, event.clientX, event.clientY);
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
