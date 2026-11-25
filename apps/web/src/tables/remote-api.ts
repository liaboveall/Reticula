import { tableDataUrl } from "@reticula/protocol/collab-api";
import { TableError, type TableFailure, type TableFailureCode } from "./failure.ts";

/** The tables of a workbook on the server, as somebody may have them. */
export interface RemoteTarget {
  readonly docId: string;
  /** Whose copies of them there are in this browser. Empty for those who were given a link. */
  readonly userId: string;
  readonly shareToken?: string | null | undefined;
  /** Whether they may be changed. Copies are only kept for those who may. */
  readonly canWrite: boolean;
}

/** The server cannot do now what it may do after a while, in milliseconds if it said so. */
export class Later extends Error {
  readonly afterMs: number | null;

  constructor(afterMs: number | null = null) {
    super();
    this.afterMs = afterMs;
  }
}

export const JSON_TYPE = { "Content-Type": "application/json" };

/** Where the server has a table. Its bytes are at `data` there. */
export function tableUrl(docId: string, id: string): string {
  return tableDataUrl(docId, id).replace(/\/data$/, "");
}

/** What is in an answer, not all of which may come. */
export function whole<T>(body: Promise<T>): Promise<T> {
  return body.catch(() => {
    throw new Later();
  });
}

/** What a status says. A quota has the same as a right that is missing. */
const CODES: Readonly<Record<number, TableFailureCode>> = {
  401: "denied",
  403: "denied",
  404: "notFound",
  413: "fileTooLarge",
  416: "changed",
  422: "corrupt",
  507: "serverFull",
};

/** Why the server did not do what it was asked. */
export function remoteFailure(status: number, detail: string): TableFailure {
  const code = /quota|as many/.test(detail) ? "accountFull" : (CODES[status] ?? "refused");
  return { code, detail: `${status} ${detail}` };
}

/**
 * Asks the server. Throws `Later` when it is worth asking again, and otherwise
 * why not, unless the status is one of those expected.
 */
export async function send(
  { shareToken }: Pick<RemoteTarget, "shareToken">,
  url: string,
  init: RequestInit,
  expected: readonly number[] = [],
): Promise<Response> {
  let response: Response;
  try {
    response = await fetch(url, {
      ...init,
      credentials: "same-origin",
      redirect: "error",
      headers: { ...init.headers, ...(shareToken ? { "X-Reticula-Share": shareToken } : {}) },
    });
  } catch {
    init.signal?.throwIfAborted();
    throw new Later();
  }
  const { status, ok, headers } = response;
  if (ok || expected.includes(status)) {
    return response;
  }
  if (status === 429 || (status >= 500 && status !== 507)) {
    throw new Later(Number(headers.get("Retry-After")) * 1_000 || null);
  }
  throw new TableError(remoteFailure(status, await response.text().catch(() => "")));
}
