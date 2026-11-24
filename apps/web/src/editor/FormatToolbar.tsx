import {
  DARK_PALETTE_FILL,
  DARK_PALETTE_TEXT,
  LIGHT_PALETTE_FILL,
  LIGHT_PALETTE_TEXT,
} from "@reticula/grid";
import type { SheetInfo, WireCellStyle, WireColorValue } from "@reticula/protocol";
import { type KeyboardEvent, type ReactNode, useLayoutEffect, useRef, useState } from "react";
import type { ColorTarget } from "../commands/registry.ts";
import { TITLES, type TitledCommand } from "../commands/titles.ts";
import { t } from "../i18n/index.ts";
import { ActionButton } from "../ui/ActionButton.tsx";
import { ListboxPopover } from "../ui/ListboxPopover.tsx";
import { useMedia } from "../ui/media.ts";
import { Popover } from "../ui/Popover.tsx";
import { freezeChoiceOf } from "./freeze.ts";
import { laterNow, lazyPart, withKeys } from "./load-later.ts";
import { keepFocus } from "./ZoomControl.tsx";

const LazyColors = lazyPart(({ Colors }) => Colors);

export interface FormatToolbarProps {
  /** The sheet in view, or `null` while there is none to work on. */
  readonly sheet: SheetInfo | null;
  readonly readOnly: boolean;
  /** Whether there are selected cells that may be formatted. */
  readonly canFormat: boolean;
  readonly canDeleteCols: boolean;
  /** The format of the active cell, as far as it is known. */
  readonly style: WireCellStyle | undefined;
  readonly formatCode: string;
  readonly onCommand: (id: string) => void;
}

const ALIGNMENTS = [
  ["alignLeft", "left", "⫷"],
  ["alignCenter", "center", "≡"],
  ["alignRight", "right", "⫸"],
] as const;

/**
 * One stop of the Tab key, with the arrow keys going from one control to the
 * next. Returns a ref for the toolbar and what it does on a key.
 */
function useRoving() {
  const ref = useRef<HTMLDivElement>(null);
  const stop = useRef<Element | null>(null);
  const controls = () => [
    ...(ref.current?.querySelectorAll<HTMLElement>(":scope > button, :scope > div > button") ?? []),
  ];
  const settle = (next: Element | null) => {
    const all = controls();
    stop.current = all.find((one) => one === next) ?? all[0] ?? null;
    for (const one of all) {
      one.tabIndex = one === stop.current ? 0 : -1;
    }
  };
  useLayoutEffect(() => settle(stop.current));
  const onKeyDown = (event: KeyboardEvent) => {
    const all = controls();
    const at = event.target instanceof HTMLElement ? all.indexOf(event.target) : -1;
    if (at < 0 || event.ctrlKey || event.metaKey || event.altKey) {
      return;
    }
    const { length } = all;
    const to = {
      ArrowRight: (at + 1) % length,
      ArrowLeft: (at - 1 + length) % length,
      Home: 0,
      End: length - 1,
    }[event.key];
    if (to !== undefined) {
      event.preventDefault();
      all[to]?.focus();
    }
  };
  return {
    ref,
    onKeyDown,
    onFocus: (event: { target: Element }) => {
      if (controls().some((one) => one === event.target)) {
        settle(event.target);
      }
    },
  };
}

const slotOf = (color: WireColorValue | undefined) =>
  color?.kind === "palette" ? color.slot & 7 : undefined;

const divider = <span className="fmt-divider" aria-hidden="true" />;

export function FormatToolbar({
  sheet,
  readOnly,
  canFormat,
  canDeleteCols,
  style,
  formatCode,
  onCommand,
}: FormatToolbarProps) {
  const isDark = useMedia("(prefers-color-scheme: dark)");
  const roving = useRoving();
  const [openColors, setOpenColors] = useState<ColorTarget | null>(null);
  // Until the names of the others are there, these are told by what is usual.
  const formats = laterNow()?.numberFormats() ?? [];
  const freezes = laterNow()?.freezeChoices() ?? [];
  const freeze = freezeChoiceOf(sheet);
  const isOpen = sheet !== null;
  const mayEdit = isOpen && !readOnly;

  const tool = (
    command: TitledCommand,
    testId: string,
    inactive: boolean,
    face: ReactNode,
    pressed?: boolean,
  ) => (
    <ActionButton
      key={command}
      inactive={inactive}
      className={pressed ? "fmt-btn active" : "fmt-btn"}
      data-testid={testId}
      aria-pressed={pressed}
      aria-label={TITLES[command]()}
      title={withKeys(TITLES[command](), command)}
      onClick={() => onCommand(command)}
    >
      {face}
    </ActionButton>
  );

  const text = (
    command: string,
    testId: string,
    inactive: boolean,
    name: string,
    pressed?: boolean,
  ) => (
    <ActionButton
      inactive={inactive}
      className={pressed ? "fmt-btn active" : "fmt-btn"}
      data-testid={testId}
      aria-pressed={pressed}
      title={withKeys(name, command)}
      onClick={() => onCommand(command)}
    >
      {name}
    </ActionButton>
  );

  const color = (
    target: ColorTarget,
    testId: string,
    label: string,
    glyph: string,
    palette: readonly string[],
    bar: string,
  ) => {
    const chosen = slotOf(style?.[target]);
    return (
      <Popover
        label={label}
        open={openColors === target}
        onOpenChange={(open) => setOpenColors(open ? target : null)}
        layout={{ columns: 4 }}
        className="color-popover"
        testId={`${testId}-popover`}
        trigger={(props) => (
          <button
            {...props}
            type="button"
            className="fmt-btn"
            data-testid={`${testId}-btn`}
            aria-disabled={!canFormat || undefined}
            aria-label={label}
            title={label}
          >
            <span className="color-glyph">{glyph}</span>
            <span
              className="color-swatch-bar"
              style={{ background: (chosen === undefined ? undefined : palette[chosen]) ?? bar }}
            />
          </button>
        )}
      >
        <LazyColors
          target={target}
          palette={palette}
          chosen={chosen}
          onChoose={(command) => {
            setOpenColors(null);
            onCommand(command);
          }}
        />
      </Popover>
    );
  };

  return (
    // A click leaves the keyboard where it is, which is most often the grid.
    <div
      {...roving}
      className="formatbar"
      role="toolbar"
      aria-label={t("Cell formatting and freeze panes")}
      onMouseDown={keepFocus}
    >
      <ListboxPopover
        label={t("Number format")}
        options={formats}
        value={formats.some(({ value }) => value === formatCode) ? formatCode : "General"}
        placeholder={t("General")}
        inactive={!canFormat}
        testId="number-format-select"
        onSelect={(code) => onCommand(`numberFormat:${code}`)}
      />
      {divider}
      {tool("bold", "fmt-bold", !canFormat, <strong>B</strong>, style?.bold ?? false)}
      {tool("italic", "fmt-italic", !canFormat, <em>I</em>, style?.italic ?? false)}
      {tool("strikethrough", "fmt-strike", !canFormat, <s>S</s>, style?.strikethrough ?? false)}
      {divider}
      {ALIGNMENTS.map(([command, align, glyph]) =>
        tool(command, `align-${align}`, !canFormat, glyph, style?.align === align),
      )}
      {divider}
      {color(
        "textColor",
        "text-color",
        t("Text color"),
        "A",
        isDark ? DARK_PALETTE_TEXT : LIGHT_PALETTE_TEXT,
        "currentColor",
      )}
      {color(
        "fillColor",
        "fill-color",
        t("Fill color"),
        "▨",
        isDark ? DARK_PALETTE_FILL : LIGHT_PALETTE_FILL,
        "var(--shell-line)",
      )}
      {divider}
      {tool("borderThin", "border-thin", !canFormat, `⊞ ${t("Thin|border")}`)}
      {tool("borderMedium", "border-medium", !canFormat, `⊞ ${t("Med|border")}`)}
      {tool("borderNone", "border-none", !canFormat, `⊟ ${t("Clear|border")}`)}
      {divider}
      <ListboxPopover
        label={t("Freeze panes")}
        options={freezes.map((choice) => ({ ...choice, testId: `freeze-${choice.value}` }))}
        value={freeze}
        placeholder={freeze === "none" ? t("No Freeze") : t("Freeze panes")}
        inactive={!mayEdit}
        testId="freeze-select"
        onSelect={(choice) => onCommand(`freeze:${choice}`)}
      />
      {divider}
      {tool("insertColLeft", "insert-col-left", !mayEdit, `+ ${t("Col")}`)}
      {tool("deleteCols", "delete-cols", !isOpen || !canDeleteCols, `− ${t("Col")}`)}
      {divider}
      {text("sort", "m3a-sort-btn", !mayEdit, t("Sort"))}
      {text("filter", "m3a-filter-btn", !isOpen, t("Filter"), sheet?.filter !== undefined)}
      {text("find", "m3a-find-btn", !isOpen, t("Find"))}
      {text("names", "m3a-names-btn", !isOpen, t("Names"))}
    </div>
  );
}
