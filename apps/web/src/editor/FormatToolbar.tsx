import {
  DARK_PALETTE_FILL,
  DARK_PALETTE_TEXT,
  LIGHT_PALETTE_FILL,
  LIGHT_PALETTE_TEXT,
} from "@reticula/grid";
import type {
  SheetInfo,
  WireCellStyle,
  WireFormatPatch,
  WireHorizontalAlign,
  WireStylePatch,
} from "@reticula/protocol";
import { useEffect, useState } from "react";
import { t } from "../i18n/index.ts";
import type { ColorPopoverKind, FreezeChoice } from "./types.ts";

export interface FormatPatch {
  readonly numberFormat?: WireFormatPatch;
  readonly style?: WireStylePatch;
}

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
  readonly colorPopover: ColorPopoverKind | null;
  readonly onColorPopover: (
    next: ColorPopoverKind | null | ((current: ColorPopoverKind | null) => ColorPopoverKind | null),
  ) => void;
  readonly onFormat: (patch: FormatPatch) => void;
  readonly onFreeze: (choice: FreezeChoice) => void;
  readonly onInsertColLeft: () => void;
  readonly onDeleteCols: () => void;
  readonly onSort: () => void;
  readonly onToggleFilter: () => void;
  readonly onFind: () => void;
  readonly onNames: () => void;
}

function numberFormats(): readonly { readonly code: string; readonly name: string }[] {
  return [
    { code: "General", name: t("General") },
    { code: "0", name: t("Integer (0)") },
    { code: "0.00", name: t("Number (0.00)") },
    { code: "#,##0.00", name: t("Thousands (#,##0.00)") },
    { code: "$#,##0.00", name: t("USD ($#,##0.00)") },
    { code: "¥#,##0.00", name: t("CNY (¥#,##0.00)") },
    { code: "€#,##0.00", name: t("EUR (€#,##0.00)") },
    { code: "0%", name: t("Percent (0%)") },
    { code: "0.00%", name: t("Percent (0.00%)") },
    { code: "0.00E+00", name: t("Scientific (0.00E+00)") },
    { code: "yyyy-mm-dd", name: t("Date (yyyy-mm-dd)") },
    { code: "yyyy-mm-dd hh:mm", name: t("DateTime (yyyy-mm-dd hh:mm)") },
    { code: "@", name: t("Plain Text (@)") },
  ];
}

const ALIGNMENTS = ["left", "center", "right"] as const satisfies readonly WireHorizontalAlign[];

const ALIGNMENT_GLYPHS = { left: "⫷", center: "≡", right: "⫸" } as const;

function alignmentTitle(align: (typeof ALIGNMENTS)[number]): string {
  switch (align) {
    case "left":
      return t("Align left");
    case "center":
      return t("Align center");
    case "right":
      return t("Align right");
  }
}

function freezeChoiceOf(sheet: SheetInfo | null): FreezeChoice {
  if (!sheet || (sheet.frozenRows === 0 && sheet.frozenCols === 0)) {
    return "none";
  }
  if (sheet.frozenRows === 1 && sheet.frozenCols === 0) {
    return "row1";
  }
  if (sheet.frozenRows === 0 && sheet.frozenCols === 1) {
    return "col1";
  }
  return sheet.frozenRows === 1 && sheet.frozenCols === 1 ? "row1col1" : "active";
}

function usePrefersDark(): boolean {
  const [isDark, setIsDark] = useState<boolean>(() =>
    typeof window !== "undefined"
      ? window.matchMedia("(prefers-color-scheme: dark)").matches
      : false,
  );
  useEffect(() => {
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const onChange = (e: MediaQueryListEvent) => setIsDark(e.matches);
    mq.addEventListener("change", onChange);
    return () => mq.removeEventListener("change", onChange);
  }, []);
  return isDark;
}

export function FormatToolbar({
  sheet,
  readOnly,
  canFormat,
  canDeleteCols,
  style,
  formatCode,
  colorPopover,
  onColorPopover,
  onFormat,
  onFreeze,
  onInsertColLeft,
  onDeleteCols,
  onSort,
  onToggleFilter,
  onFind,
  onNames,
}: FormatToolbarProps) {
  const isDark = usePrefersDark();
  const textPalette = isDark ? DARK_PALETTE_TEXT : LIGHT_PALETTE_TEXT;
  const fillPalette = isDark ? DARK_PALETTE_FILL : LIGHT_PALETTE_FILL;
  const formats = numberFormats();

  return (
    <div className="formatbar" role="toolbar" aria-label={t("Cell formatting and freeze panes")}>
      <select
        className="fmt-select"
        data-testid="number-format-select"
        aria-label={t("Number format")}
        disabled={!canFormat}
        value={formats.some((o) => o.code === formatCode) ? formatCode : "General"}
        onChange={(e) => {
          const code = e.target.value;
          onFormat({
            numberFormat: code === "General" ? { kind: "clear" } : { kind: "set", code },
          });
        }}
      >
        {formats.map((format) => (
          <option key={format.code} value={format.code}>
            {format.name}
          </option>
        ))}
      </select>

      <span className="fmt-divider" aria-hidden="true" />

      <button
        type="button"
        className={style?.bold ? "fmt-btn active" : "fmt-btn"}
        data-testid="fmt-bold"
        aria-pressed={style?.bold ?? false}
        disabled={!canFormat}
        title={t("Bold")}
        onClick={() => onFormat({ style: { bold: !(style?.bold ?? false) } })}
      >
        <strong>B</strong>
      </button>
      <button
        type="button"
        className={style?.italic ? "fmt-btn active" : "fmt-btn"}
        data-testid="fmt-italic"
        aria-pressed={style?.italic ?? false}
        disabled={!canFormat}
        title={t("Italic")}
        onClick={() => onFormat({ style: { italic: !(style?.italic ?? false) } })}
      >
        <em>I</em>
      </button>
      <button
        type="button"
        className={style?.strikethrough ? "fmt-btn active" : "fmt-btn"}
        data-testid="fmt-strike"
        aria-pressed={style?.strikethrough ?? false}
        disabled={!canFormat}
        title={t("Strikethrough")}
        onClick={() => onFormat({ style: { strikethrough: !(style?.strikethrough ?? false) } })}
      >
        <span style={{ textDecoration: "line-through" }}>S</span>
      </button>

      <span className="fmt-divider" aria-hidden="true" />

      {ALIGNMENTS.map((align) => {
        const isPressed = style?.align === align;
        return (
          <button
            key={align}
            type="button"
            className={isPressed ? "fmt-btn active" : "fmt-btn"}
            data-testid={`align-${align}`}
            aria-pressed={isPressed}
            disabled={!canFormat}
            title={alignmentTitle(align)}
            onClick={() => onFormat({ style: { align: isPressed ? "auto" : align } })}
          >
            {ALIGNMENT_GLYPHS[align]}
          </button>
        );
      })}

      <span className="fmt-divider" aria-hidden="true" />

      <div className="color-popover-wrap">
        <button
          type="button"
          className="fmt-btn"
          data-testid="text-color-btn"
          disabled={!canFormat}
          title={t("Text color")}
          onClick={() => onColorPopover((cur) => (cur === "text" ? null : "text"))}
        >
          <span className="color-glyph">A</span>
          <span
            className="color-swatch-bar"
            style={{
              background:
                style?.textColor?.kind === "palette"
                  ? (textPalette[style.textColor.slot & 7] ?? "currentColor")
                  : "currentColor",
            }}
          />
        </button>
        {colorPopover === "text" && (
          <div className="color-popover" data-testid="text-color-popover">
            <button
              type="button"
              className="color-clear-btn"
              data-testid="text-color-clear"
              onClick={() => {
                onColorPopover(null);
                onFormat({ style: { textColor: { kind: "clear" } } });
              }}
            >
              {t("Default text color")}
            </button>
            <div className="color-grid">
              {textPalette.map((hex, slot) => (
                <button
                  key={hex}
                  type="button"
                  className="color-swatch"
                  data-testid={`text-color-slot-${slot}`}
                  style={{ background: hex }}
                  aria-label={t("Text palette slot {n}", { n: slot + 1 })}
                  onClick={() => {
                    onColorPopover(null);
                    onFormat({
                      style: { textColor: { kind: "set", color: { kind: "palette", slot } } },
                    });
                  }}
                />
              ))}
            </div>
          </div>
        )}
      </div>

      <div className="color-popover-wrap">
        <button
          type="button"
          className="fmt-btn"
          data-testid="fill-color-btn"
          disabled={!canFormat}
          title={t("Fill color")}
          onClick={() => onColorPopover((cur) => (cur === "fill" ? null : "fill"))}
        >
          <span className="color-glyph">▨</span>
          <span
            className="color-swatch-bar"
            style={{
              background:
                style?.fillColor?.kind === "palette"
                  ? (fillPalette[style.fillColor.slot & 7] ?? "transparent")
                  : "var(--shell-line)",
            }}
          />
        </button>
        {colorPopover === "fill" && (
          <div className="color-popover" data-testid="fill-color-popover">
            <button
              type="button"
              className="color-clear-btn"
              data-testid="fill-color-clear"
              onClick={() => {
                onColorPopover(null);
                onFormat({ style: { fillColor: { kind: "clear" } } });
              }}
            >
              {t("No fill")}
            </button>
            <div className="color-grid">
              {fillPalette.map((hex, slot) => (
                <button
                  key={hex}
                  type="button"
                  className="color-swatch"
                  data-testid={`fill-color-slot-${slot}`}
                  style={{ background: hex }}
                  aria-label={t("Fill palette slot {n}", { n: slot + 1 })}
                  onClick={() => {
                    onColorPopover(null);
                    onFormat({
                      style: { fillColor: { kind: "set", color: { kind: "palette", slot } } },
                    });
                  }}
                />
              ))}
            </div>
          </div>
        )}
      </div>

      <span className="fmt-divider" aria-hidden="true" />

      <button
        type="button"
        className="fmt-btn"
        data-testid="border-thin"
        disabled={!canFormat}
        title={t("Thin borders")}
        onClick={() =>
          onFormat({
            style: {
              borderTop: "thin",
              borderRight: "thin",
              borderBottom: "thin",
              borderLeft: "thin",
            },
          })
        }
      >
        ⊞ {t("Thin|border")}
      </button>
      <button
        type="button"
        className="fmt-btn"
        data-testid="border-medium"
        disabled={!canFormat}
        title={t("Medium borders")}
        onClick={() =>
          onFormat({
            style: {
              borderTop: "medium",
              borderRight: "medium",
              borderBottom: "medium",
              borderLeft: "medium",
            },
          })
        }
      >
        ⊞ {t("Med|border")}
      </button>
      <button
        type="button"
        className="fmt-btn"
        data-testid="border-none"
        disabled={!canFormat}
        title={t("Clear borders")}
        onClick={() =>
          onFormat({
            style: {
              borderTop: "none",
              borderRight: "none",
              borderBottom: "none",
              borderLeft: "none",
            },
          })
        }
      >
        ⊟ {t("Clear|border")}
      </button>

      <span className="fmt-divider" aria-hidden="true" />

      <select
        className="fmt-select"
        data-testid="freeze-select"
        aria-label={t("Freeze panes")}
        disabled={!sheet || readOnly}
        value={freezeChoiceOf(sheet)}
        onChange={(e) => onFreeze(e.target.value as FreezeChoice)}
      >
        <option value="none">{t("No Freeze")}</option>
        <option value="row1">{t("Freeze Top Row")}</option>
        <option value="col1">{t("Freeze First Column")}</option>
        <option value="row1col1">{t("Freeze Row 1 & Col A")}</option>
        <option value="active">{t("Freeze to Active Cell")}</option>
      </select>

      <span className="fmt-divider" aria-hidden="true" />

      <button
        type="button"
        className="fmt-btn"
        data-testid="insert-col-left"
        disabled={!sheet || readOnly}
        title={t("Insert 1 column to the left of selection")}
        onClick={onInsertColLeft}
      >
        + {t("Col")}
      </button>
      <button
        type="button"
        className="fmt-btn"
        data-testid="delete-cols"
        disabled={!sheet || !canDeleteCols}
        title={t("Delete selected columns")}
        onClick={onDeleteCols}
      >
        − {t("Col")}
      </button>
      <span className="fmt-divider" />
      <button
        type="button"
        className="fmt-btn"
        data-testid="m3a-sort-btn"
        disabled={!sheet || readOnly}
        onClick={onSort}
      >
        {t("Sort")}
      </button>
      <button
        type="button"
        className={sheet?.filter ? "fmt-btn active" : "fmt-btn"}
        data-testid="m3a-filter-btn"
        disabled={!sheet}
        onClick={onToggleFilter}
      >
        {t("Filter")}
      </button>
      <button
        type="button"
        className="fmt-btn"
        data-testid="m3a-find-btn"
        disabled={!sheet}
        onClick={onFind}
      >
        {t("Find")}
      </button>
      <button
        type="button"
        className="fmt-btn"
        data-testid="m3a-names-btn"
        disabled={!sheet}
        onClick={onNames}
      >
        {t("Names")}
      </button>
    </div>
  );
}
