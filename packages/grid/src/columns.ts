import type { FilterButtonRect } from "./filter.ts";
import type { ColumnHeaders, HeaderCell } from "./render/frame.ts";

// The headers of columns that have names, loaded by whoever has such columns.
// What it needs of the grid it is handed: importing it would have the bundler
// take the grid apart, and compress the parts worse.

export interface NamedColumn {
  readonly name: string;
  readonly sort?: "ascending" | "descending" | null;
  /** Which of several columns that the rows are sorted by this is, from 1. */
  readonly sortOrder?: number | undefined;
  /** Whether rows are left out by what is in this column. */
  readonly filtered?: boolean;
}

// Sizes in pixels at a zoom of 1.
const PADDING = 6;
const BUTTON = 16;
/** Wider than the edge that the column is resized by. */
const MARGIN = 5;
/** The least of a header that is left besides its button: some of the name has to be. */
const LEAST = 24;
const ARROW = 10;
const GAP = 3;
/** The room for a digit of the order, about. */
const DIGIT = 8;

const ASCENDING = [
  [0, -4.5],
  [3.5, -0.5],
  [0.75, -0.5],
  [0.75, 4.5],
  [-0.75, 4.5],
  [-0.75, -0.5],
  [-3.5, -0.5],
] as const;
const DESCENDING = ASCENDING.map(([x, y]) => [x, -y] as const);

const ELLIPSIS = "\u{2026}";
/** More characters of a name than the widest column has room for. Names come out of files. */
const MOST = 512;

/** What a name was shortened to, and for what. */
interface Shortened {
  readonly font: string;
  readonly room: number;
  readonly text: string;
}

export class NamedColumns implements ColumnHeaders {
  #columns: readonly NamedColumn[];
  #shortened = new Map<number, Shortened>();

  constructor(columns: readonly NamedColumn[]) {
    this.#columns = columns;
  }

  nameOf(col: number): string {
    return this.#columns[col]?.name ?? "";
  }

  sortOf(col: number): "ascending" | "descending" | null {
    return this.#columns[col]?.sort ?? null;
  }

  roomOf(col: number): number {
    const order = this.#orderOf(col);
    const arrow = this.sortOf(col) ? ARROW + GAP + DIGIT * order.length : 0;
    return BUTTON + MARGIN - PADDING + arrow;
  }

  buttonIn(left: number, width: number, height: number, scale: number): FilterButtonRect | null {
    const size = BUTTON * scale;
    if (width < size + (MARGIN + LEAST) * scale || height < size) {
      return null;
    }
    return { x: left + width - size - MARGIN * scale, y: (height - size) / 2, size };
  }

  draw(cell: HeaderCell): void {
    const { col, left, right, bottom, scale, look, font, measure } = cell;
    const pixels = (css: number) => Math.round(css * scale);
    let end = right - pixels(GAP);

    const button = this.buttonIn(left, right - left, bottom, scale);
    if (button) {
      const x = Math.round(button.x);
      const y = Math.round(button.y);
      const size = Math.round(button.size);
      const filtered = this.#columns[col]?.filtered === true;
      if (filtered) {
        const box = { left: x, top: y, right: x + size, bottom: y + size };
        cell.fill(box, look.selectionFill);
      }
      cell.sprite(
        {
          kind: filtered ? "funnel" : "chevron",
          width: size,
          height: size,
          scale,
          color: filtered ? look.filterIconActive : look.filterIcon,
        },
        x,
        y,
      );
      end = x - pixels(GAP);
    }

    const start = left + pixels(PADDING);
    const sort = this.sortOf(col);
    if (sort) {
      const order = this.#orderOf(col);
      const orderWidth = order ? Math.ceil(measure.width(font, order)) : 0;
      const size = pixels(ARROW);
      const x = end - orderWidth - size;
      if (x >= left) {
        const corners = sort === "ascending" ? ASCENDING : DESCENDING;
        cell.sprite(
          { kind: "shape", width: size, height: size, scale, color: look.filterIcon, corners },
          x,
          Math.round((bottom - size) / 2),
        );
        if (order) {
          cell.text(order, x + size, end);
        }
        end = x - pixels(GAP);
      }
    }

    const text = this.#fit(cell, end - start);
    if (text) {
      cell.text(text, start, end);
    }
  }

  #orderOf(col: number): string {
    return String(this.#columns[col]?.sortOrder ?? "");
  }

  /** As much of a column's name as there is room for. */
  #fit({ col, font, measure }: HeaderCell, room: number): string {
    const name = this.nameOf(col).slice(0, MOST);
    if (measure.width(font, name) <= room) {
      return name;
    }
    const known = this.#shortened.get(col);
    if (known?.font === font && known.room === room) {
      return known.text;
    }
    const left = room - measure.width(font, ELLIPSIS);
    // The most characters that fit, and the fewest that do not.
    let fits = 0;
    let over = name.length;
    while (over - fits > 1) {
      const middle = (fits + over) >> 1;
      if (measure.width(font, name.slice(0, middle)) <= left) {
        fits = middle;
      } else {
        over = middle;
      }
    }
    // Not half of a character, and no space before what stands for the rest.
    const kept = name
      .slice(0, fits)
      .replace(/[\u{d800}-\u{dbff}]$/u, "")
      .trimEnd();
    const text = left < 0 ? "" : kept + ELLIPSIS;
    this.#shortened.set(col, { font, room, text });
    return text;
  }
}
