import { IS_MAC, ZOOM_STEPS } from "@reticula/grid";
import type { RefObject } from "react";
import { flushSync } from "react-dom";
import type { EditorMenu } from "../editor/types.ts";
import { fmtInt, t } from "../i18n/index.ts";
import Popup, { type PopupPlace } from "../ui/Popup.tsx";
import type { EditorHandle } from "./handle.ts";
import { keysOf, writtenKeysOf } from "./key-names.ts";
import { type ColorTarget, colorNames, commandOf, isEnabled, run } from "./registry.ts";

/** The keys of a command as assistive technology has them. */
function keyShortcuts(id: string): string | undefined {
  const keys = writtenKeysOf(id)
    .replace("Mod", IS_MAC ? "Meta" : "Control")
    .replace("Ctrl", "Control");
  return keys || undefined;
}

/** An entry of a menu: a command, and how tests find it. A line is between two groups. */
type Entry = readonly [id: string, testId?: string, title?: string] | "line";

interface CommandMenuProps {
  readonly handle: EditorHandle;
  readonly label: string;
  readonly entries: readonly Entry[];
  /** The worksheet the commands are about, if it need not be the one in view. */
  readonly sheet?: number | undefined;
  readonly anchor?: RefObject<HTMLElement | null> | undefined;
  readonly at: PopupPlace;
  readonly onClose: () => void;
  readonly className?: string | undefined;
  readonly testId: string;
}

function CommandMenu({
  handle,
  label,
  entries,
  sheet,
  className,
  onClose,
  ...popup
}: CommandMenuProps) {
  const state = handle.state();
  return (
    <Popup
      {...popup}
      kind="menu"
      label={label}
      layout={{ wrap: true }}
      className={className ? `context-menu ${className}` : "context-menu"}
      onClose={onClose}
    >
      {entries.map((entry, at) => {
        if (entry === "line") {
          // biome-ignore lint/suspicious/noArrayIndexKey: lines have nothing to tell them apart
          return <hr key={at} className="ctx-sep" />;
        }
        const [id, testId, title] = entry;
        const command = commandOf(id);
        if (!command) {
          return null;
        }
        const inactive = !isEnabled(command, state, sheet);
        const keys = keysOf(id);
        const attributes = {
          role: command.checked ? "menuitemradio" : "menuitem",
          "aria-checked": command.checked?.(handle),
          "aria-disabled": inactive || undefined,
          "aria-keyshortcuts": keyShortcuts(id),
        };
        return (
          <button
            {...attributes}
            key={id}
            type="button"
            tabIndex={-1}
            className={id === "deleteSheet" ? "ctx-item danger" : "ctx-item"}
            data-testid={testId ?? `menu-${id}`}
            onClick={() => {
              if (!inactive) {
                // The focus is back where it was when the command has it.
                flushSync(onClose);
                run(handle, id, sheet);
              }
            }}
          >
            {title ?? command.title()}
            {keys && (
              <span className="ctx-keys" aria-hidden="true">
                {keys}
              </span>
            )}
          </button>
        );
      })}
    </Popup>
  );
}

const CLIPBOARD: readonly Entry[] = [
  ["cut", "ctx-cut"],
  ["copy", "ctx-copy"],
  ["paste", "ctx-paste"],
  ["clearContents", "ctx-clear"],
];

const ROWS: readonly Entry[] = [
  ["insertRowAbove", "ctx-insert-row-above"],
  ["insertRowBelow", "ctx-insert-row-below"],
  ["deleteRows", "ctx-delete-rows"],
];

const COLS: readonly Entry[] = [
  ["insertColLeft", "ctx-insert-col-left"],
  ["insertColRight", "ctx-insert-col-right"],
  ["deleteCols", "ctx-delete-cols"],
];

const ENTRIES: Readonly<Record<"cell" | "rowHeader" | "colHeader", readonly Entry[]>> = {
  cell: [...CLIPBOARD, "line", ...ROWS, "line", ...COLS, "line", ["filterColumn"]],
  rowHeader: [
    ...CLIPBOARD,
    "line",
    ...ROWS,
    "line",
    ["moveRowsUp"],
    ["moveRowsDown"],
    ["rowHeight"],
  ],
  colHeader: [
    ...CLIPBOARD,
    "line",
    ...COLS,
    "line",
    ["moveColsLeft"],
    ["moveColsRight"],
    ["colWidth"],
    ["autofitCols"],
    "line",
    ["filterColumn"],
  ],
};

const TAB: readonly Entry[] = [
  ["renameSheet", "ctx-rename-sheet"],
  ["moveSheetLeft", "ctx-move-sheet-left"],
  ["moveSheetRight", "ctx-move-sheet-right"],
  "line",
  ["deleteSheet", "ctx-delete-sheet"],
];

/** Where the menu of what is selected goes: below its first cell, as far as that is in view. */
function placeOfSelection(handle: EditorHandle): PopupPlace {
  const host = document.querySelector(".sheet .reticula-grid")?.getBoundingClientRect();
  const box = handle.grid()?.boxOfSelection();
  if (!host || !box) {
    return { x: host?.left ?? 0, y: host?.top ?? 0 };
  }
  const within = (position: number, size: number) => Math.max(0, Math.min(position, size));
  return {
    x: host.left + within(box.left + Math.min(box.width, 80) / 2, host.width),
    y: host.top + within(box.top + Math.min(box.height, 24), host.height),
  };
}

export interface EditorMenusProps {
  readonly handle: EditorHandle;
  readonly menu: EditorMenu;
  readonly onClose: () => void;
}

/** The menu of some cells, of rows or columns, or of a worksheet's tab. */
export function EditorMenus({ handle, menu, onClose }: EditorMenusProps) {
  const common = { handle, onClose, testId: "context-menu" };
  if (menu.kind === "tab") {
    const { tab, clientX: x, clientY: y } = menu;
    return (
      <CommandMenu
        {...common}
        label={tab.name}
        entries={TAB}
        sheet={tab.id}
        at={{ x, y, above: true }}
      />
    );
  }
  const scope = handle.grid()?.selectionScope;
  const zone =
    menu.kind === "grid"
      ? menu.target.zone
      : scope === "rows"
        ? "rowHeader"
        : scope === "cols"
          ? "colHeader"
          : "cell";
  return (
    <CommandMenu
      {...common}
      label={t("Selection")}
      entries={ENTRIES[zone]}
      at={
        menu.kind === "grid"
          ? { x: menu.target.clientX, y: menu.target.clientY }
          : placeOfSelection(handle)
      }
    />
  );
}

export interface AnchoredMenuProps {
  readonly handle: EditorHandle;
  readonly anchor: RefObject<HTMLElement | null>;
  readonly onClose: () => void;
}

function placeOf(anchor: HTMLElement | null, above: boolean): PopupPlace {
  const box = anchor?.getBoundingClientRect();
  return { x: box?.left ?? 0, y: above ? (box?.top ?? 0) - 4 : (box?.bottom ?? 0) + 4, above };
}

const VIEW: readonly Entry[] = [
  ["zoomIn"],
  ["zoomOut"],
  ["zoom:100"],
  "line",
  ["freeze:none"],
  ["freeze:row1"],
  ["freeze:col1"],
  ["freeze:row1col1"],
  ["freeze:active"],
  "line",
  ["rendererAuto"],
  ["rendererCanvas"],
  "line",
  ["language:en"],
  ["language:zh-CN"],
  "line",
  ["palette"],
  ["keyboardHelp"],
];

export function ViewMenu({ anchor, ...props }: AnchoredMenuProps) {
  return (
    <CommandMenu
      {...props}
      label={t("View")}
      entries={VIEW}
      anchor={anchor}
      at={placeOf(anchor.current, false)}
      testId="view-menu"
    />
  );
}

export function ZoomMenu({ anchor, ...props }: AnchoredMenuProps) {
  return (
    <CommandMenu
      {...props}
      label={t("Zoom")}
      entries={ZOOM_STEPS.map((zoom): Entry => {
        const percent = Math.round(zoom * 100);
        return [`zoom:${percent}`, `zoom-${percent}`, `${fmtInt(percent)}%`];
      })}
      anchor={anchor}
      at={placeOf(anchor.current, true)}
      className="zoom-menu"
      testId="zoom-menu"
    />
  );
}

export interface ColorsProps {
  readonly target: ColorTarget;
  readonly palette: readonly string[];
  /** The chosen one's place in the palette, if it is one of them. */
  readonly chosen: number | undefined;
  readonly onChoose: (command: string) => void;
}

/** The colors to choose from, and none of them. */
export function Colors({ target, palette, chosen, onChoose }: ColorsProps) {
  const testId = target === "textColor" ? "text-color" : "fill-color";
  const none = commandOf(`${target}:none`);
  return (
    <>
      <div className="color-grid">
        {colorNames().map((name, slot) => (
          <button
            key={name}
            type="button"
            tabIndex={-1}
            className="color-swatch"
            style={{ background: palette[slot] }}
            aria-label={name}
            aria-pressed={slot === chosen}
            title={name}
            data-testid={`${testId}-slot-${slot}`}
            onClick={() => onChoose(`${target}:${slot}`)}
          />
        ))}
      </div>
      <button
        type="button"
        tabIndex={-1}
        className="color-clear-btn"
        data-testid={`${testId}-clear`}
        onClick={() => onChoose(`${target}:none`)}
      >
        {none?.title()}
      </button>
    </>
  );
}
