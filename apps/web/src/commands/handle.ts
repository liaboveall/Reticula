import type { CellEditor, Grid, RemotePresenceOverlay, Selection, Span } from "@reticula/grid";
import type { EngineCommand, SheetInfo, SheetTabInfo, WireCellStyle } from "@reticula/protocol";
import type { CollabWorkbookTarget } from "../collab/CollabShell.tsx";
import type { SheetAction } from "../editor/activity.ts";
import type { EditorDialog, EditorMenu } from "../editor/types.ts";
import type { EngineMessage, HostCommand } from "../engine/client.ts";
import type { M3aToolMode } from "../M3aToolsModal.tsx";

/** What the commands that there are depend on. */
export interface EditorState {
  /** The sheet in view, or `null` while there is none to work on. */
  readonly sheet: SheetInfo | null;
  readonly readOnly: boolean;
  readonly selection: Selection | null;
  /** The format of the active cell, as far as it is known. */
  readonly style: WireCellStyle | undefined;
  readonly formatCode: string;
  /** Whether the workbook is one on the server. */
  readonly isCloud: boolean;
}

/** What has been copied last, for browsers that do not let the clipboard be read. */
export interface CopiedSnapshot {
  readonly sheet: number;
  readonly firstRow: number;
  readonly rowCount: number;
  readonly firstCol: number;
  readonly colCount: number;
  readonly cut: boolean;
  readonly tsv: string;
  readonly html: string;
}

export interface MutateOptions {
  /** What the status bar says about it. */
  readonly action?: SheetAction;
  /** Whether it computes anything, for which the engine is told the time. */
  readonly computes?: boolean;
}

/** The editor, for its commands. */
export interface EditorHandle {
  state(): EditorState;
  /** The sheet the engine has in view, which may not have been rendered yet. */
  sheetId(): number;
  /** The same, or the one it is about to have in view. */
  askedSheetId(): number;
  grid(): Grid | null;
  cellEditor(): CellEditor | null;
  /** Sends a change. `false` if nothing may be changed now. */
  mutate(command: EngineCommand, options?: MutateOptions): boolean;
  /** Sends what changes nothing. `false` if there is no engine. */
  send(command: EngineCommand): boolean;
  sendHost(command: HostCommand, transfer?: Transferable[]): void;
  /** Opens a workbook, in an engine of its own if it is to be or there is none. */
  openWith(docId: string, command: HostCommand, isFresh?: boolean): void;
  openCollab(target: CollabWorkbookTarget): void;
  collab(): CollabWorkbookTarget | null;
  updateCollab(change: Partial<CollabWorkbookTarget>): void;
  /** Where the others in the workbook are. */
  presences(): readonly RemotePresenceOverlay[];
  /** Asks the browser to keep what is stored, now that there is something worth it. */
  noteChange(): void;
  /** Continues what is in a part of some cells over the rest of them. */
  fill(src: Span, dst: Span): boolean;
  /** Continues it as far down as there is something beside it. */
  autoFillDown(src: Span): boolean;
  /** Sees what the engine sends until it returns `true` for it, or until it is stopped. */
  intercept(listener: (message: EngineMessage) => boolean): () => void;
  /** Accepts what is being typed into a cell. The focus stays where it is. */
  commitPending(): void;
  copied(): CopiedSnapshot | null;
  switchSheet(sheet: number): void;
  /** Does something once a sheet is in view, which it may be already: else, what has come. */
  onSheet(sheet: number, then: (grid: Grid, arrived?: SheetInfo) => void): void;
  createWorkbook(title: string, template: "empty" | "sample"): void;
  /** Opens an empty workbook. */
  newWorkbook(): void;
  /** What a workbook that is being made is known by. */
  freshDocId(): string;
  openTool(mode: M3aToolMode): void;
  openWorkbooks(importing?: boolean): void;
  openCloud(): void;
  toggleHistory(): void;
  toggleAi(): void;
  openDialog(dialog: EditorDialog): void;
  openMenu(menu: EditorMenu): void;
  renameSheet(tab: SheetTabInfo): void;
  editNameBox(): void;
}
