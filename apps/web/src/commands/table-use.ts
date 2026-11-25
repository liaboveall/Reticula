/**
 * What becomes of a command while a data table is in view:
 * - `table`: it is for the table;
 * - `same`: what it always does;
 * - `off`: nothing, as all the others are about worksheets and their cells.
 */
export type TableUse = "off" | "table" | "same";

// Families of commands, such as those for the zoom, are told by what is before the
// colon of their names.
const words = (text: string) => new Set(text.split(" "));
const TABLE = words(
  "copy selectRows selectCols selectAll colWidth autofitCols freeze:none freeze:col1 " +
    "freeze:active filterColumn clearFilters zoomIn zoomOut zoom contextMenu",
);
const SAME = words(
  "goTo resumeRecalc addSheet nextSheet previousSheet rendererAuto rendererCanvas language " +
    "nextRegion previousRegion workbooks newWorkbook import importTable sampleTable exportRtcl " +
    "exportXlsx cloud history ai palette keyboardHelp",
);

export function tableUseOf(id: string): TableUse {
  const is = (commands: ReadonlySet<string>) =>
    commands.has(id) || commands.has(id.replace(/:.*/, ""));
  return id.startsWith("table") || is(TABLE) ? "table" : is(SAME) ? "same" : "off";
}
