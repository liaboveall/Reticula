import { t } from "../i18n/index.ts";

/** What the commands are called that the editor starts with a button for. */
export const TITLES = {
  undo: () => t("Undo"),
  redo: () => t("Redo"),
  insertRowAbove: () => t("Insert Row Above"),
  deleteRows: () => t("Delete Selected Rows"),
  insertColLeft: () => t("Insert Column Left"),
  deleteCols: () => t("Delete Selected Columns"),
  bold: () => t("Bold"),
  italic: () => t("Italic"),
  strikethrough: () => t("Strikethrough"),
  alignLeft: () => t("Align left"),
  alignCenter: () => t("Align center"),
  alignRight: () => t("Align right"),
  borderThin: () => t("Thin borders"),
  borderMedium: () => t("Medium borders"),
  borderNone: () => t("Clear borders"),
} as const;

export type TitledCommand = keyof typeof TITLES;
