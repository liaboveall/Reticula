import { ApiError } from "@reticula/protocol/collab-api";
import { translateEngineText } from "../i18n/engine-text.ts";
import { t } from "../i18n/index.ts";
import { errorText } from "./errors.ts";

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

/** What to tell the user about a failed request. `fallback` is in the language shown. */
export function apiErrorMessage(err: unknown, fallback: string): string {
  if (err instanceof ApiError) {
    switch (err.status) {
      case 401:
        return t("Sign in required to perform this action.");
      case 403:
        return t("Access denied or account storage quota reached.");
      case 409:
        return t("A duplicate item with that name already exists.");
      case 413:
        return t("Uploaded file exceeds the 32 MiB collaborative import limit.");
      case 422:
        return t("Workbook exceeds collaborative limits (2M cells / 64 MiB) or invalid input.");
      case 429:
        return t("Too many requests right now. Please try again in a moment.");
      case 503:
        return t("Server document memory budget is temporarily busy.");
      case 507:
        return t("Server storage volume is full; delete unused workbooks or try later.");
      default:
        return t("{message} (HTTP {status})", { message: fallback, status: err.status });
    }
  }
  return translateEngineText(errorText(err, fallback));
}
