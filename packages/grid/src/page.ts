import {
  CELL_FLAGS_AT,
  CELL_LEN,
  CELL_STYLE_AT,
  CELL_TEXT_LEN_AT,
  CELL_TEXT_START_AT,
  COL_COUNT_AT,
  EPOCH_AT,
  FIRST_COL_AT,
  FIRST_ROW_AT,
  FLAG_BOOL,
  FLAG_ERROR,
  FLAG_FORMULA,
  FLAG_NUMBER,
  FLAG_SPILL,
  FLAG_TRUNCATED,
  HEADER_LEN,
  ID_LEN,
  OVERFLOW_COUNT_AT,
  OVERFLOW_LEN,
  OVERFLOW_ORIGIN_COL_AT,
  OVERFLOW_ROW_OFFSET_AT,
  OVERFLOW_STYLE_AT,
  OVERFLOW_TEXT_LEN_AT,
  OVERFLOW_TEXT_START_AT,
  ROW_COUNT_AT,
  SHEET_AT,
  TEXT_LEN_AT,
  VERSION,
  VERSION_AT,
} from "@reticula/protocol/viewport";

/**
 * Left-aligned text originating to the left of `firstCol` that may visually
 * overflow across empty cells into this page (`14-m1b.md` 6.2).
 */
export interface LeftOverflow {
  /** Row index within this page (`0 .. rowCount - 1`). */
  readonly rowOffset: number;
  /** 0-based visible column position `< firstCol` where the text sits. */
  readonly originCol: number;
  /** `StyleId` of the originating cell. */
  readonly styleId: number;
  /** Display string of the originating cell. */
  readonly text: string;
}

/**
 * A viewport page v2 as the grid keeps it: a block of cells with formatted
 * display text, `StyleId`s, flags, row/col identities, and left-overflow records.
 */
export interface Page {
  /** Identifier of the sheet (`SheetId`). */
  readonly sheet: number;
  /** The subscription epoch the engine sent this page for. */
  readonly epoch: number;
  /** Position of the first row. */
  readonly firstRow: number;
  /** Position of the first column. */
  readonly firstCol: number;
  readonly rowCount: number;
  readonly colCount: number;
  /** Identifier of each row, from `firstRow` on. */
  readonly rowIds: BigUint64Array;
  /** Identifier of each column, from `firstCol` on. */
  readonly colIds: BigUint64Array;
  /** Text of each cell, row by row. An empty cell has empty text. */
  readonly texts: readonly string[];
  /** `StyleId` of each cell (`0` = default style), row by row. */
  readonly styleIds: Uint16Array;
  /** Flags of each cell, row by row. */
  readonly flags: Uint16Array;
  /** Left-overflow text candidates when `firstCol > 0`. */
  readonly overflows: readonly LeftOverflow[];
  /** Size of the packet the page came in. */
  readonly byteLength: number;
}

/** Whether a cell with these flags holds a number. */
export function isNumber(flags: number): boolean {
  return (flags & FLAG_NUMBER) !== 0;
}

/** Whether a cell with these flags holds a boolean (`TRUE` / `FALSE`). */
export function isBool(flags: number): boolean {
  return (flags & FLAG_BOOL) !== 0;
}

/** Whether a cell with these flags holds an error (`#DIV/0!`, `#REF!`, ...). */
export function isError(flags: number): boolean {
  return (flags & FLAG_ERROR) !== 0;
}

/** Whether a cell with these flags is backed by a bound formula. */
export function isFormula(flags: number): boolean {
  return (flags & FLAG_FORMULA) !== 0;
}

/** Whether a cell's display text was truncated at the viewport cap. */
export function isTruncated(flags: number): boolean {
  return (flags & FLAG_TRUNCATED) !== 0;
}

/** Whether a cell displays a materialized dynamic array spill shadow value. */
export function isSpill(flags: number): boolean {
  return (flags & FLAG_SPILL) !== 0;
}

// A text may start with the mark of the byte order; the records count it like any other character.
const utf8 = new TextDecoder("utf-8", { ignoreBOM: true });

/**
 * Reads a v2 viewport page from the binary packet sent by the engine worker.
 */
export function decodePage(buffer: ArrayBuffer): Page {
  if (buffer.byteLength < HEADER_LEN) {
    throw new Error(`viewport page: ${buffer.byteLength} bytes is too short for a header`);
  }
  const view = new DataView(buffer);
  const version = view.getUint32(VERSION_AT, true);
  if (version !== VERSION) {
    throw new Error(`viewport page: layout version ${version}, expected ${VERSION}`);
  }
  const sheet = view.getUint32(SHEET_AT, true);
  const rowCount = view.getUint16(ROW_COUNT_AT, true);
  const colCount = view.getUint16(COL_COUNT_AT, true);
  const textLength = view.getUint32(TEXT_LEN_AT, true);
  const overflowCount = view.getUint16(OVERFLOW_COUNT_AT, true);
  const cellCount = rowCount * colCount;

  const rowIdsAt = HEADER_LEN;
  const colIdsAt = rowIdsAt + rowCount * ID_LEN;
  const cellsAt = colIdsAt + colCount * ID_LEN;
  const overflowsAt = cellsAt + cellCount * CELL_LEN;
  const textAt = overflowsAt + overflowCount * OVERFLOW_LEN;
  if (buffer.byteLength !== textAt + textLength) {
    throw new Error(
      `viewport page: ${buffer.byteLength} bytes, but its header describes ${textAt + textLength}`,
    );
  }

  const rowIds = new BigUint64Array(rowCount);
  for (let row = 0; row < rowCount; row++) {
    rowIds[row] = view.getBigUint64(rowIdsAt + row * ID_LEN, true);
  }
  const colIds = new BigUint64Array(colCount);
  for (let col = 0; col < colCount; col++) {
    colIds[col] = view.getBigUint64(colIdsAt + col * ID_LEN, true);
  }

  const text = utf8.decode(new Uint8Array(buffer, textAt, textLength));
  const texts = new Array<string>(cellCount);
  const styleIds = new Uint16Array(cellCount);
  const flags = new Uint16Array(cellCount);
  let end = 0;
  for (let cell = 0; cell < cellCount; cell++) {
    const record = cellsAt + cell * CELL_LEN;
    const start = view.getUint32(record + CELL_TEXT_START_AT, true);
    end = start + view.getUint16(record + CELL_TEXT_LEN_AT, true);
    texts[cell] = start === end ? "" : text.substring(start, end);
    styleIds[cell] = view.getUint16(record + CELL_STYLE_AT, true);
    flags[cell] = view.getUint16(record + CELL_FLAGS_AT, true);
  }

  const overflows = new Array<LeftOverflow>(overflowCount);
  for (let idx = 0; idx < overflowCount; idx++) {
    const record = overflowsAt + idx * OVERFLOW_LEN;
    const start = view.getUint32(record + OVERFLOW_TEXT_START_AT, true);
    end = start + view.getUint16(record + OVERFLOW_TEXT_LEN_AT, true);
    overflows[idx] = {
      rowOffset: view.getUint16(record + OVERFLOW_ROW_OFFSET_AT, true),
      originCol: view.getUint16(record + OVERFLOW_ORIGIN_COL_AT, true),
      styleId: view.getUint16(record + OVERFLOW_STYLE_AT, true),
      text: start === end ? "" : text.substring(start, end),
    };
  }

  if (end !== text.length) {
    throw new Error(
      `viewport page: records account for ${end} code units of text, the text has ${text.length}`,
    );
  }

  return {
    sheet,
    epoch: view.getUint32(EPOCH_AT, true),
    firstRow: view.getUint32(FIRST_ROW_AT, true),
    firstCol: view.getUint32(FIRST_COL_AT, true),
    rowCount,
    colCount,
    rowIds,
    colIds,
    texts,
    styleIds,
    flags,
    overflows,
    byteLength: buffer.byteLength,
  };
}
