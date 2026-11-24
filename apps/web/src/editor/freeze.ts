import type { SheetInfo } from "@reticula/protocol";
import type { FreezeChoice } from "./types.ts";

export function freezeChoiceOf(sheet: SheetInfo | null): FreezeChoice {
  const { frozenRows = 0, frozenCols = 0 } = sheet ?? {};
  if (frozenRows > 1 || frozenCols > 1) {
    return "active";
  }
  return frozenRows === 0
    ? frozenCols === 0
      ? "none"
      : "col1"
    : frozenCols === 0
      ? "row1"
      : "row1col1";
}
