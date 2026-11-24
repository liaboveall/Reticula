import { fmtInt, fmtMs, t } from "../i18n/index.ts";

/** What a command did, for the status bar. */
export type SheetAction =
  | "updated"
  | "updatedCell"
  | "clearedCells"
  | "pasted"
  | "filledRange"
  | "autoFilledColumn"
  | "formattedCells"
  | "insertedRows"
  | "deletedRows"
  | "movedRows"
  | "resizedRows"
  | "insertedColumns"
  | "deletedColumns"
  | "movedColumns"
  | "resizedColumns"
  | "updatedFreezePanes"
  | "switchedSheet"
  | "addedSheet"
  | "renamedSheet"
  | "reorderedSheets"
  | "deletedSheet"
  | "undidAction"
  | "redidAction";

/** The latest the engine has done or is doing. The status bar puts it into words. */
export type Activity =
  | {
      readonly kind: "done";
      readonly action: SheetAction;
      readonly ms: number;
      readonly recalculated: number;
    }
  | { readonly kind: "recalculating"; readonly done: number; readonly total: number }
  | { readonly kind: "recalculated"; readonly cells: number; readonly ms: number }
  | { readonly kind: "replaced"; readonly cells: number };

export function actionText(action: SheetAction): string {
  switch (action) {
    case "updated":
      return t("updated");
    case "updatedCell":
      return t("updated cell");
    case "clearedCells":
      return t("cleared cells");
    case "pasted":
      return t("pasted");
    case "filledRange":
      return t("filled range");
    case "autoFilledColumn":
      return t("auto-filled column");
    case "formattedCells":
      return t("formatted cells");
    case "insertedRows":
      return t("inserted rows");
    case "deletedRows":
      return t("deleted rows");
    case "movedRows":
      return t("moved rows");
    case "resizedRows":
      return t("resized rows");
    case "insertedColumns":
      return t("inserted columns");
    case "deletedColumns":
      return t("deleted columns");
    case "movedColumns":
      return t("moved columns");
    case "resizedColumns":
      return t("resized columns");
    case "updatedFreezePanes":
      return t("updated freeze panes");
    case "switchedSheet":
      return t("switched sheet");
    case "addedSheet":
      return t("added sheet");
    case "renamedSheet":
      return t("renamed sheet");
    case "reorderedSheets":
      return t("reordered sheets");
    case "deletedSheet":
      return t("deleted sheet");
    case "undidAction":
      return t("undid action");
    case "redidAction":
      return t("redid action");
  }
}

export function activityText(activity: Activity): string {
  switch (activity.kind) {
    case "done": {
      const params = { action: actionText(activity.action), time: fmtMs(activity.ms) };
      return activity.recalculated > 0
        ? t("{action} in {time}, recalculated {cells} cells", {
            ...params,
            cells: fmtInt(activity.recalculated),
          })
        : t("{action} in {time}", params);
    }
    case "recalculating":
      return t("recalculating {done} / {total}…", {
        done: fmtInt(activity.done),
        total: fmtInt(activity.total),
      });
    case "recalculated":
      return t("recalculated {cells} cells in {time}", {
        cells: fmtInt(activity.cells),
        time: fmtMs(activity.ms),
      });
    case "replaced":
      return t("replaced {cells} cell(s)", { cells: fmtInt(activity.cells) });
  }
}
