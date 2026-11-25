import type { TableTrouble } from "@reticula/protocol/table";

/**
 * Why something about tables could not be done. Codes, which are for the one who
 * shows them to say in words.
 */
export type TableFailureCode =
  // What the engine says.
  | TableTrouble["reason"]
  // Of the place the tables are in.
  | "unsupported"
  | "blocked"
  | "yielded"
  | "closed"
  | "notFound"
  | "inUse"
  | "quota"
  // Of what was asked for.
  | "busy"
  | "cancelled"
  | "superseded"
  | "invalidName"
  | "duplicateName"
  | "tooLarge"
  // Of the server.
  /** It may not be had, or not be changed, by whoever asks. */
  | "denied"
  /** The server cannot be reached, and not all that is needed is in this browser. */
  | "offline"
  /** It is still being uploaded. */
  | "uploading"
  /** Whoever the workbook belongs to has no room for it on the server. */
  | "accountFull"
  /** The server has no room for it. */
  | "serverFull"
  /** It is larger than the server takes. */
  | "fileTooLarge"
  /** The server does not take it, for a reason that `detail` has. */
  | "refused"
  // Of the worker itself.
  | "crashed"
  | "load";

export interface TableFailure {
  readonly code: TableFailureCode;
  /** For whoever looks into it. */
  readonly detail?: string | undefined;
  /** The row of the text at which it arose. */
  readonly row?: number | undefined;
  /** The column that what was asked does not fit. */
  readonly column?: number | undefined;
}

export class TableError extends Error {
  override readonly name = "TableError";
  readonly code: TableFailureCode;
  readonly failure: TableFailure;

  constructor(failure: TableFailure | TableFailureCode) {
    const whole = typeof failure === "string" ? { code: failure } : failure;
    super(whole.detail ? `${whole.code}: ${whole.detail}` : whole.code);
    this.code = whole.code;
    this.failure = whole;
  }
}
