import type { Grid } from "@reticula/grid";
import { type RefObject, useEffect } from "react";
import { t } from "../i18n/index.ts";

/** Hands a grid its texts, again whenever they read differently. */
export function useGridStrings(grid: RefObject<Grid | null>): void {
  const name = t("Worksheet grid");
  const cellEditor = t("Cell editor");
  const selectedCell = t("Selected cell");
  useEffect(() => {
    grid.current?.setStrings({ grid: name, cellEditor, selectedCell });
  }, [grid, name, cellEditor, selectedCell]);
}
