import { clampZoom, type GridView, type SelectionScope } from "@reticula/grid";

/** Where what was in view of which sheet is kept, in this browser. */
const STORAGE_KEY = "reticula:view:v1";
/** Documents remembered; the ones left longest ago are forgotten first. */
const MOST_DOCUMENTS = 50;
/** Scrolling and selecting are written down once they have paused for this long. */
const PAUSE_MS = 500;

type Views = ReadonlyMap<number, GridView>;

interface StoredDocument {
  readonly id: string;
  readonly sheets: Readonly<Record<string, unknown>>;
}

const SCOPES: readonly SelectionScope[] = ["cells", "rows", "cols", "all"];

type Cell = { readonly row: number; readonly col: number };

const isRecord = (value: unknown): value is Readonly<Record<string, unknown>> =>
  typeof value === "object" && value !== null;

const isPosition = (value: unknown): value is number =>
  typeof value === "number" && Number.isSafeInteger(value) && value >= 0;

const isFraction = (value: unknown): value is number =>
  typeof value === "number" && value >= 0 && value <= 1;

const isCell = (value: unknown): value is Cell =>
  isRecord(value) && isPosition(value.row) && isPosition(value.col);

function parseSelection(value: unknown): GridView["selection"] {
  if (!isRecord(value) || !isCell(value.anchor) || !isCell(value.focus)) {
    return null;
  }
  const { anchor, focus } = value;
  const scope = SCOPES.find((known) => known === value.scope) ?? "cells";
  return { anchor, focus, scope };
}

/** A view as it was stored, or `null` for anything else. */
export function parseView(value: unknown): GridView | null {
  if (!isRecord(value)) {
    return null;
  }
  const { zoom, row, rowFraction, col, colFraction } = value;
  return typeof zoom === "number" &&
    clampZoom(zoom, Number.NaN) === zoom &&
    isPosition(row) &&
    isFraction(rowFraction) &&
    isPosition(col) &&
    isFraction(colFraction)
    ? { zoom, row, rowFraction, col, colFraction, selection: parseSelection(value.selection) }
    : null;
}

/** The documents' views in a browser's storage, which may be full, forbidden or tampered with. */
export class ViewStore {
  #storage: () => Storage;

  constructor(storage: () => Storage = () => window.localStorage) {
    this.#storage = storage;
  }

  read(documentId: string): Map<number, GridView> {
    const views = new Map<number, GridView>();
    const stored = this.#documents().find(({ id }) => id === documentId);
    for (const [sheet, value] of Object.entries(stored?.sheets ?? {})) {
      const view = parseView(value);
      if (view && isPosition(Number(sheet))) {
        views.set(Number(sheet), view);
      }
    }
    return views;
  }

  /** Remembers a sheet's view. Those of other sheets may have changed in another tab. */
  write(documentId: string, sheet: number, view: GridView): void {
    const documents = this.#documents();
    const stored = documents.find(({ id }) => id === documentId);
    const others = documents.filter(({ id }) => id !== documentId).slice(1 - MOST_DOCUMENTS);
    const own = { id: documentId, sheets: { ...stored?.sheets, [sheet]: view } };
    // Without room for all of them, for this one at least.
    for (const kept of [others, []]) {
      try {
        this.#storage().setItem(STORAGE_KEY, JSON.stringify({ documents: [...kept, own] }));
        return;
      } catch {
        // The next visit starts at the top then.
      }
    }
  }

  #documents(): StoredDocument[] {
    let parsed: unknown;
    try {
      parsed = JSON.parse(this.#storage().getItem(STORAGE_KEY) ?? "null");
    } catch {
      return [];
    }
    if (!isRecord(parsed) || !Array.isArray(parsed.documents)) {
      return [];
    }
    return parsed.documents.flatMap((entry: unknown) =>
      isRecord(entry) && typeof entry.id === "string" && isRecord(entry.sheets)
        ? [{ id: entry.id, sheets: entry.sheets }]
        : [],
    );
  }
}

/**
 * What was in view of the sheets that have been open. Past versions and drafts
 * are looked at from where the document was left, and leave no trace.
 */
export class ViewMemory {
  #grid: { readonly view: () => GridView };
  #store: ViewStore;
  #open: { readonly documentId: string; readonly sheet: number } | null = null;
  #documentId: string | null = null;
  #views = new Map<number, GridView>();
  /** The views of a preview's sheets, while there is one. */
  #preview: Map<number, GridView> | null = null;
  #timer: ReturnType<typeof setTimeout> | undefined;
  #written = "";

  constructor(grid: { readonly view: () => GridView }, store = new ViewStore()) {
    this.#grid = grid;
    this.#store = store;
  }

  /** The view to open a sheet with. What the grid shows until then is remembered. */
  open(documentId: string, sheet: number, isPreview: boolean): Partial<GridView> {
    this.save();
    if (documentId !== this.#documentId) {
      this.#documentId = documentId;
      this.#views = this.#store.read(documentId);
      this.#preview = null;
    }
    this.#preview = isPreview ? (this.#preview ?? new Map(this.#views)) : null;
    this.#open = { documentId, sheet };
    this.#written = "";
    const views: Views = this.#preview ?? this.#views;
    return views.get(sheet) ?? { zoom: 1 };
  }

  /** To be called when the view may have changed. */
  touch(): void {
    if (this.#open) {
      clearTimeout(this.#timer);
      this.#timer = setTimeout(() => this.save(), PAUSE_MS);
    }
  }

  /** Remembers what the grid shows, now. */
  save(): void {
    clearTimeout(this.#timer);
    this.#timer = undefined;
    if (!this.#open) {
      return;
    }
    const { documentId, sheet } = this.#open;
    const view = this.#grid.view();
    if (this.#preview) {
      this.#preview.set(sheet, view);
      return;
    }
    this.#views.set(sheet, view);
    const written = JSON.stringify(view);
    if (written !== this.#written) {
      this.#written = written;
      this.#store.write(documentId, sheet, view);
    }
  }
}
