import type {
  BranchDiff,
  ClosureIssue,
  ConflictResolution,
  MergePreviewResponse,
} from "@reticula/protocol/collab-api";
import { type Message, t } from "../i18n/index.ts";

export interface ReplayIndexEntry {
  readonly id: string;
  readonly title: string;
  readonly description: string;
  readonly prompt: string;
  readonly tags: readonly string[];
  readonly hasConflictDemo: boolean;
  readonly hasClosureDemo: boolean;
  readonly stepCount: number;
}

export interface ReplayIndex {
  readonly version: number;
  readonly generatedWith: string;
  readonly items: readonly ReplayIndexEntry[];
}

export interface ReplayPresenceHint {
  readonly sheet: number;
  readonly row: number;
  readonly col: number;
  readonly endRow: number;
  readonly endCol: number;
}

export interface ReplayStep {
  readonly idx: number;
  readonly kind: string;
  readonly title: string;
  readonly detail: string;
  readonly presence?: ReplayPresenceHint | null;
  readonly txnB64?: string | null;
}

export interface ReplayHunkOps {
  readonly applyTxnsB64: readonly string[];
  readonly refSanitizedTxnsB64?: readonly string[];
}

export interface ReplayClosureDep {
  readonly sourceHunkId: string;
  readonly targetHunkId: string;
  readonly issue: ClosureIssue;
}

export interface ReplayBundle {
  readonly id: string;
  readonly title: string;
  readonly description: string;
  readonly prompt: string;
  readonly summary: string;
  readonly highlights: readonly string[];
  readonly baseSnapshotB64: string;
  readonly mainConcurrentTxnsB64?: readonly string[];
  readonly steps: readonly ReplayStep[];
  readonly diff: BranchDiff;
  readonly hunkOps: Readonly<Record<string, ReplayHunkOps>>;
  readonly closureDependencies?: readonly ReplayClosureDep[];
  readonly acceptAllTxnsB64?: readonly string[];
}

/** A recording, or the list of them, that the server does not give out. */
export class ReplayLoadError extends Error implements Message {
  readonly say: () => string;

  constructor(say: () => string) {
    super(say());
    this.say = say;
  }
}

export async function fetchReplayIndex(): Promise<ReplayIndex> {
  const resp = await fetch("/data/ai-replays/index.json");
  const { ok, status } = resp;
  if (!ok) {
    const say = () => t("Could not load AI demos ({status})", { status });
    throw new ReplayLoadError(say);
  }
  return resp.json();
}

export async function fetchReplayBundle(id: string): Promise<ReplayBundle> {
  const resp = await fetch(`/data/ai-replays/${encodeURIComponent(id)}.json`);
  const { ok, status } = resp;
  if (!ok) {
    const say = () => t('Could not load demo "{id}" ({status})', { id, status });
    throw new ReplayLoadError(say);
  }
  return resp.json();
}

export function decodeBase64(b64: string): ArrayBuffer {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i += 1) {
    out[i] = bin.charCodeAt(i);
  }
  return out.buffer;
}

/** Evaluates unresolved conflicts and static reference closure locally for a ReplayBundle. */
export function previewLocalReplayMerge(
  bundle: ReplayBundle,
  acceptedHunks: ReadonlySet<string>,
  conflictChoices: Readonly<Record<string, ConflictResolution>>,
  allowRefErrors: boolean,
): MergePreviewResponse {
  const unresolvedConflicts: string[] = [];
  const hunkById = new Map(bundle.diff.hunks.map((h) => [h.id, h]));

  for (const h of bundle.diff.hunks) {
    if (!acceptedHunks.has(h.id)) continue;
    for (const c of h.cells) {
      if (c.conflict && !conflictChoices[c.cellKey]) {
        unresolvedConflicts.push(c.cellKey);
      }
    }
  }

  const closureIssues: ClosureIssue[] = [];
  for (const dep of bundle.closureDependencies ?? []) {
    if (!acceptedHunks.has(dep.sourceHunkId) || acceptedHunks.has(dep.targetHunkId)) {
      continue;
    }
    const sourceHunk = hunkById.get(dep.sourceHunkId);
    if (
      sourceHunk?.hasConflict &&
      !sourceHunk.cells.some((c) => conflictChoices[c.cellKey] === "take_draft")
    ) {
      continue;
    }
    closureIssues.push(dep.issue);
  }

  const canMerge =
    unresolvedConflicts.length === 0 && (closureIssues.length === 0 || allowRefErrors);
  return {
    canMerge,
    mainHeadSeq: bundle.diff.mainHeadSeq,
    closureIssues,
    unresolvedConflicts,
    estimatedOps: acceptedHunks.size,
  };
}

/** Assembles ordered WireTxnV3 buffers (`mainConcurrent` + selected hunks) for local merge. */
export function buildLocalReplayMergeTxns(
  bundle: ReplayBundle,
  acceptedHunks: ReadonlySet<string>,
  conflictChoices: Readonly<Record<string, ConflictResolution>>,
  allowRefErrors: boolean,
): ArrayBuffer[] {
  const out: ArrayBuffer[] = [];
  for (const b64 of bundle.mainConcurrentTxnsB64 ?? []) {
    out.push(decodeBase64(b64));
  }

  const allAccepted =
    bundle.diff.hunks.every((h) => {
      if (!acceptedHunks.has(h.id)) return false;
      if (!h.hasConflict) return true;
      return h.cells.every((c) => !c.conflict || conflictChoices[c.cellKey] === "take_draft");
    }) && (bundle.acceptAllTxnsB64?.length ?? 0) > 0;
  if (allAccepted && bundle.acceptAllTxnsB64) {
    for (const b64 of bundle.acceptAllTxnsB64) {
      out.push(decodeBase64(b64));
    }
    return out;
  }

  const depsBySource = new Map<string, ReplayClosureDep[]>();
  for (const dep of bundle.closureDependencies ?? []) {
    const list = depsBySource.get(dep.sourceHunkId) ?? [];
    list.push(dep);
    depsBySource.set(dep.sourceHunkId, list);
  }

  // Track structural hunks already bundled inside an accepted dependent hunk's applyTxnsB64.
  const coveredStructTargets = new Set<string>();
  for (const h of bundle.diff.hunks) {
    if (!acceptedHunks.has(h.id)) continue;
    const deps = depsBySource.get(h.id) ?? [];
    if (deps.length > 0 && deps.every((d) => acceptedHunks.has(d.targetHunkId))) {
      for (const d of deps) coveredStructTargets.add(d.targetHunkId);
    }
  }

  for (const h of bundle.diff.hunks) {
    if (!acceptedHunks.has(h.id) || coveredStructTargets.has(h.id)) continue;
    if (h.hasConflict) {
      const wantDraft = h.cells.some((c) => conflictChoices[c.cellKey] === "take_draft");
      if (!wantDraft) continue;
    }
    const ops = bundle.hunkOps[h.id];
    if (!ops) continue;
    const deps = depsBySource.get(h.id) ?? [];
    const missingDep = deps.some((d) => !acceptedHunks.has(d.targetHunkId));
    const list =
      missingDep && allowRefErrors && ops.refSanitizedTxnsB64?.length
        ? ops.refSanitizedTxnsB64
        : ops.applyTxnsB64;
    for (const b64 of list) {
      out.push(decodeBase64(b64));
    }
  }
  return out;
}
