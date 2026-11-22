import { ApiError } from "@reticula/protocol/collab-api";

export type AppRoute =
  | "landing"
  | "app"
  | "workbooks"
  | "account"
  | "privacy"
  | "sim"
  | "benchmarks";

export function normalizeAppRoute(pathname: string): AppRoute {
  if (pathname === "/app" || pathname.startsWith("/app/") || pathname.startsWith("/join/")) {
    return "app";
  }
  if (pathname === "/workbooks" || pathname.startsWith("/workbooks/")) {
    return "workbooks";
  }
  if (pathname === "/account" || pathname.startsWith("/account/")) {
    return "account";
  }
  if (pathname === "/privacy" || pathname.startsWith("/privacy/")) {
    return "privacy";
  }
  if (pathname === "/sim" || pathname.startsWith("/sim/")) {
    return "sim";
  }
  if (pathname === "/benchmarks" || pathname.startsWith("/benchmarks/")) {
    return "benchmarks";
  }
  return "landing";
}

export function pickNearestSnapshotSeq(
  snapshotSeqs: readonly string[],
  targetSeq: string,
): string | null {
  const target = BigInt(targetSeq);
  let best: bigint | null = null;
  for (const raw of snapshotSeqs) {
    const seq = BigInt(raw);
    if (seq <= target && (best === null || seq > best)) {
      best = seq;
    }
  }
  return best === null ? null : best.toString();
}

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) {
    return "0 B";
  }
  if (bytes < 1024) {
    return `${Math.round(bytes)} B`;
  }
  if (bytes < 1024 * 1024) {
    return `${(bytes / 1024).toFixed(1)} KB`;
  }
  if (bytes < 1024 * 1024 * 1024) {
    return `${(bytes / (1024 * 1024)).toFixed(2)} MB`;
  }
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}

export function apiErrorMessage(err: unknown, fallback: string): string {
  if (err instanceof ApiError) {
    switch (err.status) {
      case 401:
        return "Sign in required to perform this action.";
      case 403:
        return "Access denied or account storage quota reached.";
      case 409:
        return "A duplicate item with that name already exists.";
      case 413:
        return "Uploaded file exceeds the 32 MiB collaborative import limit.";
      case 422:
        return "Workbook exceeds collaborative limits (2M cells / 64 MiB) or invalid input.";
      case 429:
        return "Too many requests right now. Please try again in a moment.";
      case 503:
        return "Server document memory budget is temporarily busy.";
      case 507:
        return "Server storage volume is full; delete unused workbooks or try later.";
      default:
        return `${fallback} (HTTP ${err.status})`;
    }
  }
  return err instanceof Error ? err.message : fallback;
}
