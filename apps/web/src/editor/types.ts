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
      /** What was being typed, and where, when the workbook was taken. */
      readonly lost: { readonly cell: string; readonly text: string } | null;
    };

/** A menu that opens where something was pressed, or at what is selected. */
export type EditorMenu =
  | { readonly kind: "grid"; readonly target: GridContextTarget }
  | { readonly kind: "selection" }
  | {
      readonly kind: "tab";
      readonly clientX: number;
      readonly clientY: number;
      readonly tab: SheetTabInfo;
    };

/** A dialog of the editor's own, apart from the tools and the workbooks. */
export type EditorDialog =
  | { readonly kind: "palette" }
  | { readonly kind: "help" }
  | { readonly kind: "size"; readonly axis: "row" | "col" }
  | { readonly kind: "deleteSheet"; readonly tab: SheetTabInfo };

export type FreezeChoice = "none" | "row1" | "col1" | "row1col1" | "active";
