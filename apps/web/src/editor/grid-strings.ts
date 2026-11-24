/** The names the grid's inputs have for assistive technology. */
export interface GridStrings {
  readonly cellEditor: string;
  readonly formulaBar: string;
}

function takesStrings(
  grid: object,
): grid is { readonly setStrings: (strings: GridStrings) => void } {
  return "setStrings" in grid && typeof grid.setStrings === "function";
}

/**
 * Gives a grid its texts in the language of the page. A grid that takes none
 * has its one input named from outside.
 */
export function passGridStrings(grid: object, host: Element, strings: GridStrings): void {
  if (takesStrings(grid)) {
    grid.setStrings(strings);
  } else {
    host.querySelector('[role="textbox"]')?.setAttribute("aria-label", strings.cellEditor);
  }
}
