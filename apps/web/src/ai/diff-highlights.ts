import type { DiffHighlightKind, DiffHighlightMap } from "@reticula/grid";
import type { CellDiffEntry, DiffHunk } from "@reticula/protocol/collab-api";

/**
 * Whether the draft empties a cell. One that was empty before is not deleted:
 * a change of format lists those too.
 */
function isEmptied(cell: CellDiffEntry): boolean {
  const before = cell.baseValue || cell.baseFormula || cell.mainValue || cell.mainFormula;
  return Boolean(before) && !cell.draftValue && !cell.draftFormula;
}

/** How the grid marks the cells of a sheet that the hunks of a review are about. */
export function buildHighlights(
  hunks: readonly DiffHunk[],
  accepted: ReadonlySet<string>,
  activeSheetId: number,
): DiffHighlightMap {
  const map = new Map<string, DiffHighlightKind>();
  for (const hunk of hunks) {
    if (Number(hunk.sheetId) !== activeSheetId) continue;
    for (const cell of hunk.cells) {
      const kind: DiffHighlightKind = cell.conflict
        ? "conflict"
        : accepted.has(hunk.id) && !isEmptied(cell)
          ? "added"
          : "deleted";
      map.set(`${cell.rowPos}:${cell.colPos}`, kind);
    }
  }
  return map;
}
