/** What a grid's parts are called by assistive technology, in the language of the page. */
export interface GridStrings {
  /** The grid itself. */
  readonly grid: string;
  /** The input that what is typed into a cell goes to. */
  readonly cellEditor: string;
  /** The table that says where the selection is. */
  readonly selectedCell: string;
}

/** Until a grid has been handed its texts, which are in no language of its own. */
export const NO_STRINGS: GridStrings = { grid: "", cellEditor: "", selectedCell: "" };
