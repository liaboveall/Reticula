import type { Grid, GridStats, RemotePresenceOverlay } from "@reticula/grid";
import type { SafeModeInfo } from "./engine/client.ts";
import type { StorageBackendKind } from "./engine/storage.ts";
import { setRendererPreference } from "./renderer.ts";

export interface SwStatus {
  readonly supported: boolean;
  readonly registered: boolean;
  readonly controlled: boolean;
  readonly waiting: boolean;
  readonly buildId: string | null;
}

export interface HookDocStatus {
  readonly docId: string;
  readonly title: string;
  readonly engineStatus:
    | "starting"
    | "ready"
    | "failed"
    | "blocked"
    | "takingOver"
    | "yielded"
    | "safeMode";
  readonly storageBackend: StorageBackendKind | null;
  readonly activeSlot: "a" | "b" | null;
  readonly walSeq: string;
  readonly waitingForFile: boolean;
  readonly safeMode: SafeModeInfo | null;
}

export interface SnapshotSaveResult {
  readonly docId: string;
  readonly slot: "a" | "b";
  readonly byteLen: number;
  readonly elapsedMs: number;
}

export interface CollabHookStatus {
  readonly active: boolean;
  readonly docId: string | null;
  readonly connection: "offline" | "connecting" | "catchingUp" | "online";
  readonly replica: number | null;
  readonly pending: number;
  readonly slot: string | null;
  readonly remotePresences: readonly RemotePresenceOverlay[];
}

/**
 * A window onto the running app for end-to-end tests and measurements, which
 * assert on what the grid and the engine report instead of comparing pixels.
 *
 * It is installed only when the page is opened with `?hooks` in its address,
 * so that the build under test is the build that ships.
 */
export interface Hooks {
  /** What the grid is doing right now. */
  stats(): GridStats;
  /** Scrolls to a position in CSS pixels. */
  scrollTo(position: { top?: number; left?: number }): void;
  /** Selects a cell. */
  select(row: number, col: number): void;
  /** Selects a rectangular cell range (`anchor` to `focus`). */
  selectRange(anchor: { row: number; col: number }, focus: { row: number; col: number }): void;
  /** The text of a cell, or `null` while the grid does not hold it. */
  textAt(row: number, col: number): string | null;
  /** Runs the engine's determinism self-check and resolves to its digest. */
  selfCheck(): Promise<string>;
  /** The workbook's state digest: equal on two replicas exactly when their content is. */
  stateDigest(): Promise<string>;
  /** Current local persistence, lock, and Safe Mode status. */
  docStatus(): HookDocStatus;
  /** Triggers a background CoW snapshot compaction and resolves when flushed. */
  compactSnapshot(): Promise<SnapshotSaveResult>;
  /** Flushes and closes open OPFS handles & Web Locks cleanly. */
  flushAndClose(): Promise<void>;
  /** Busy-waits the engine worker for `ms` (for testing the 500 ms takeover watchdog). */
  spinWorker(ms: number): void;
  /** Current Service Worker offline shell registration and active build ID. */
  swStatus(): Promise<SwStatus>;
  /** Triggers an immediate Service Worker update check (`registration.update()`). */
  checkSwUpdate(): Promise<void>;
  /** Current collaborative session status and visible remote presences. */
  collabStatus(): CollabHookStatus;
  /** Forces the collaborative WebSocket transport offline or back online (for tests). */
  setCollabOffline(offline: boolean): void;
  /** Chooses what draws the grid, as the menu does. */
  setRenderer(preference: "auto" | "canvas2d"): void;
}

declare global {
  interface Window {
    __reticula?: Hooks;
  }
}

export function hooksRequested(): boolean {
  return new URLSearchParams(window.location.search).has("hooks");
}

export interface HookExtras {
  readonly selfCheck: () => Promise<string>;
  readonly stateDigest: () => Promise<string>;
  readonly docStatus: () => HookDocStatus;
  readonly compactSnapshot: () => Promise<SnapshotSaveResult>;
  readonly flushAndClose: () => Promise<void>;
  readonly spinWorker: (ms: number) => void;
  readonly swStatus: () => Promise<SwStatus>;
  readonly checkSwUpdate: () => Promise<void>;
  readonly collabStatus: () => CollabHookStatus;
  readonly setCollabOffline: (offline: boolean) => void;
}

export function installHooks(grid: Grid, extras: HookExtras): () => void {
  const hooks: Hooks = {
    stats: () => grid.stats(),
    scrollTo: (position) => grid.scrollTo(position),
    select: (row, col) => grid.select({ row, col }),
    selectRange: (anchor, focus) => grid.selectRange(anchor, focus),
    textAt: (row, col) => grid.textAt(row, col),
    selfCheck: extras.selfCheck,
    stateDigest: extras.stateDigest,
    docStatus: extras.docStatus,
    compactSnapshot: extras.compactSnapshot,
    flushAndClose: extras.flushAndClose,
    spinWorker: extras.spinWorker,
    swStatus: extras.swStatus,
    checkSwUpdate: extras.checkSwUpdate,
    collabStatus: extras.collabStatus,
    setCollabOffline: extras.setCollabOffline,
    setRenderer: (preference) => setRendererPreference(grid, preference),
  };
  window.__reticula = hooks;
  return () => {
    if (window.__reticula === hooks) {
      delete window.__reticula;
    }
  };
}
