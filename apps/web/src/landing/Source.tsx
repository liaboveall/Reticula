import type { Provenance } from "../generated/benchmarks.ts";
import { t } from "../i18n/index.ts";
import { commitUrl, OUTSIDE, sourceUrl } from "./links.ts";

export interface SourceProps {
  readonly of: Provenance | null;
  /** What had a part in the result: the machine, and the browser on it, or neither. */
  readonly ran?: "in a browser" | "natively" | "anywhere";
}

/** Where numbers come from: what was measured when, with what and on what. */
export function Source({ of: from, ran = "in a browser" }: SourceProps) {
  if (!from) {
    return (
      <p className="landing-source" data-testid="provenance">
        {t("not available")}
      </p>
    );
  }
  const { commit, date, uncommittedChanges, browser, headless, cpu, affinity } = from;
  return (
    <ul className="landing-source" data-testid="provenance">
      <li>
        <a href={commitUrl(from)} {...OUTSIDE} aria-label={t("Commit {commit}", { commit })}>
          {commit}
        </a>
        {uncommittedChanges === true && ` (${t("uncommitted changes")})`}
      </li>
      <li>
        <time dateTime={date}>{date}</time>
      </li>
      {ran === "in a browser" && (
        <li>
          {browser ?? t("not available")}
          {headless === true && ` (${t("headless")})`}
        </li>
      )}
      {ran !== "anywhere" && (
        <li>
          {cpu ?? t("machine not recorded")}
          {affinity !== null && ` (${t("affinity {mask}", { mask: affinity })})`}
        </li>
      )}
      <li>
        <a href={sourceUrl(from.report)} {...OUTSIDE}>
          {t("report")}
        </a>
      </li>
    </ul>
  );
}
