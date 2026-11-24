import type { GridContextTarget } from "@reticula/grid";
import type { EngineInfo, SheetInfo, SheetTabInfo } from "@reticula/protocol";
import type { HookDocStatus } from "../hooks.ts";

export type EngineState =
  | { status: "starting" }
  | { status: "ready"; info: EngineInfo; sheet: SheetInfo }
  | { status: "failed"; message: string };

/** The state of the document as the status bar names it. */
export type EngineStatus = HookDocStatus["engineStatus"];

export type LockOverlayState =
  | { readonly kind: "blocked"; readonly docId: string; readonly title: string }
  | {
      readonly kind: "takingOver";
      readonly docId: string;
      readonly title: string;
      readonly waitingForFile: boolean;
    }
  | {
      readonly kind: "yielded";
      readonly docId: string;
      readonly title: string;
      readonly evicted: boolean;
    };

export type ContextMenuState =
  | { readonly kind: "grid"; readonly target: GridContextTarget }
  | {
      readonly kind: "tab";
      readonly clientX: number;
      readonly clientY: number;
      readonly tab: SheetTabInfo;
    };

export type ColorPopoverKind = "text" | "fill";

/** A sheet that is being renamed, with what has been typed so far. */
export interface SheetRename {
  readonly id: number;
  readonly draft: string;
}

export type ClipboardAction = "cut" | "copy" | "paste";

export type FreezeChoice = "none" | "row1" | "col1" | "row1col1" | "active";
