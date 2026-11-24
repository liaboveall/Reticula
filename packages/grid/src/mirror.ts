import { columnName, type SheetSize } from "./layout.ts";

/** The selected cell, at its place in the sheet itself: rows a filter hides count. */
export interface MirroredCell {
  readonly row: number;
  readonly col: number;
  readonly text: string;
}

let mirrors = 0;

type TableCell = HTMLTableCellElement;

function cell(tag: "td" | "th", scope?: "col" | "row"): TableCell {
  const element = document.createElement(tag);
  if (scope) {
    element.scope = scope;
  }
  return element;
}

function setIndex(element: TableCell | HTMLTableRowElement, axis: "row" | "col", index: number) {
  element.setAttribute(`aria-${axis}index`, String(index));
}

/**
 * The selected cell as a table, for assistive technology, which cannot see into
 * the canvas. It has no focus and takes none: that stays where the keys go.
 */
export class Mirror {
  readonly element: HTMLTableElement;
  #id = `reticula-mirror-${++mirrors}`;
  #corner = cell("td");
  #colHeader = cell("th", "col");
  #rowHeader = cell("th", "row");
  #cell = cell("td");
  #headers: HTMLTableRowElement;
  #row: HTMLTableRowElement;
  #shown = "";

  constructor() {
    this.element = document.createElement("table");
    this.element.className = "reticula-mirror";
    this.#headers = this.element.createTHead().insertRow();
    this.#row = this.element.createTBody().insertRow();
    this.#headers.append(this.#corner, this.#colHeader);
    this.#row.append(this.#rowHeader, this.#cell);
    this.element.id = this.#id;
    this.#colHeader.id = `${this.#id}-col`;
    this.#rowHeader.id = `${this.#id}-row`;
    this.#cell.headers = `${this.#colHeader.id} ${this.#rowHeader.id}`;
    setIndex(this.#headers, "row", 1);
    setIndex(this.#corner, "col", 1);
    setIndex(this.#rowHeader, "col", 1);
  }

  name(label: string): void {
    this.element.setAttribute("aria-label", label);
  }

  /** The sheet's size counts all of its rows, as the cell's place does. */
  show(sheet: SheetSize, selected: MirroredCell | null): void {
    const shown = JSON.stringify([sheet.rows, sheet.cols, selected]);
    if (shown === this.#shown) {
      return;
    }
    this.#shown = shown;
    // The headers are a row and a column of the table themselves.
    this.element.setAttribute("aria-rowcount", String(sheet.rows + 1));
    this.element.setAttribute("aria-colcount", String(sheet.cols + 1));
    this.#row.hidden = selected === null;
    this.#colHeader.hidden = selected === null;
    if (!selected) {
      return;
    }
    const { row, col, text } = selected;
    setIndex(this.#row, "row", row + 2);
    setIndex(this.#colHeader, "col", col + 2);
    setIndex(this.#cell, "col", col + 2);
    this.#colHeader.textContent = columnName(col);
    this.#rowHeader.textContent = String(row + 1);
    this.#cell.textContent = text;
  }
}
