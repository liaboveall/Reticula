import {
  CELL_FLAGS_AT,
  CELL_LEN,
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
  FLAG_TRUNCATED,
  HEADER_LEN,
  ID_LEN,
  ROW_COUNT_AT,
  SHEET_AT,
  TEXT_LEN_AT,
  VERSION,
  VERSION_AT,
} from "@reticula/protocol/viewport";

/**
 * A viewport page as the grid keeps it: a block of cells with the text each
 * one shows and the identifiers of its rows and columns.
 */
export interface Page {
  /** Identifier of the sheet (`SheetId`). */
  readonly sheet: number;
  /** The subscription the engine sent this page for. */
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
  /** Flags of each cell, row by row. */
  readonly flags: Uint8Array;
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

const utf8 = new TextDecoder("utf-8");

/**
 * Reads a page from the packet the engine sent.
 *
 * A packet that does not have the expected layout means the two sides of the
 * worker boundary disagree, which no amount of care at the call site can
 * repair; it is reported by throwing.
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
  const cellCount = rowCount * colCount;

  const rowIdsAt = HEADER_LEN;
  const colIdsAt = rowIdsAt + rowCount * ID_LEN;
  const cellsAt = colIdsAt + colCount * ID_LEN;
  const textAt = cellsAt + cellCount * CELL_LEN;
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

  // The text of all cells is decoded once; each cell is then a slice of the
  // result. That is why the records count in UTF-16 code units.
  const text = utf8.decode(new Uint8Array(buffer, textAt, textLength));
  const texts = new Array<string>(cellCount);
  const flags = new Uint8Array(cellCount);
  let end = 0;
  for (let cell = 0; cell < cellCount; cell++) {
    const record = cellsAt + cell * CELL_LEN;
    const start = view.getUint32(record + CELL_TEXT_START_AT, true);
    end = start + view.getUint16(record + CELL_TEXT_LEN_AT, true);
    texts[cell] = start === end ? "" : text.substring(start, end);
    flags[cell] = view.getUint8(record + CELL_FLAGS_AT);
  }
  if (end !== text.length) {
    throw new Error(
      `viewport page: the cells account for ${end} code units of text, the text has ${text.length}`,
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
    flags,
    byteLength: buffer.byteLength,
  };
}
