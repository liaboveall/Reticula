import type { AgentStepKind, ClosureIssue, DiffHunkKind } from "@reticula/protocol/collab-api";
import { translateEngineText } from "../i18n/engine-text.ts";
import { t } from "../i18n/index.ts";

/** The state of a run. The server names the states in English. */
export function runStatusText(status: string): string {
  switch (status) {
    case "queued":
      return t("queued|run");
    case "running":
      return t("running|run");
    case "waiting_review":
      return t("waiting_review|run");
    case "merged":
      return t("merged|run");
    case "rejected":
      return t("rejected|run");
    case "failed":
      return t("failed|run");
    case "cancelled":
      return t("cancelled|run");
    default:
      return status;
  }
}

export function stepKindText(kind: AgentStepKind): string {
  switch (kind) {
    case "user_prompt":
      return t("user_prompt|step");
    case "assistant_thought":
      return t("assistant_thought|step");
    case "tool_call":
      return t("tool_call|step");
    case "tool_result":
      return t("tool_result|step");
    case "txn_commit":
      return t("txn_commit|step");
    case "error":
      return t("error|step");
    case "finish":
      return t("finish|step");
    default:
      // A newer server may know more kinds than this page does.
      return kind;
  }
}

/** What the server sums a run up with when the model says nothing. */
const NO_SUMMARY = "Completed analysis with no workbook changes.";

/** What a run came to. The words of the model are left alone. */
export function runSummaryText(summary: string): string {
  return summary === NO_SUMMARY ? translateEngineText(summary) : summary;
}

/** What a step is about: the first that its payload has of a name, a summary, a text or a prompt. */
export function stepText(payload: unknown): string {
  if (typeof payload !== "object" || payload === null) {
    return String(payload);
  }
  const { name, summary, text, prompt } = payload as Readonly<Record<string, unknown>>;
  const content = name ?? summary ?? text ?? prompt ?? JSON.stringify(payload).slice(0, 140);
  return runSummaryText(String(content));
}

/** What a conflicting cell holds: its formula, else its value, else what became of it. */
export function contentText(formula: string | null | undefined, value: string): string {
  if (formula !== null && formula !== undefined) {
    return formula;
  }
  switch (value) {
    case "":
      return t("(empty)");
    case "<deleted row>":
      return t("<deleted row>");
    case "<deleted on main>":
      return translateEngineText(value);
    default:
      return value;
  }
}

/**
 * Where a reference is that a merge would break. A change other than of a cell
 * has its label for both the cell and the formula.
 */
export function closureSource(issue: Pick<ClosureIssue, "sourceCell" | "formulaText">): {
  readonly cell: string;
  readonly formula: string;
} {
  const { sourceCell, formulaText } = issue;
  if (sourceCell !== formulaText) {
    return { cell: sourceCell, formula: formulaText };
  }
  const label = translateEngineText(sourceCell);
  return { cell: label, formula: label };
}

export function hunkKindText(kind: DiffHunkKind): string {
  switch (kind) {
    case "structural":
      return t("structural|hunk");
    case "sort":
      return t("sort|hunk");
    case "cell_block":
      return t("cell_block|hunk");
    case "meta":
      return t("meta|hunk");
    default:
      return kind;
  }
}
