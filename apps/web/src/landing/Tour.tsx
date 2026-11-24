import type { ReactNode } from "react";
import { t } from "../i18n/index.ts";
import { Link } from "./Link.tsx";
import { OUTSIDE, REPOSITORY } from "./links.ts";
import { Picture } from "./Picture.tsx";
import type { ShotName } from "./shots.ts";

interface Stop {
  readonly shot: ShotName;
  readonly alt: string;
  readonly heading: string;
  readonly text: string;
  readonly link: ReactNode;
}

/** What there is to try, each with a picture of it. */
export function Tour({ onNavigate }: { readonly onNavigate: (path: string) => void }) {
  const page = (to: string, text: string) => (
    <Link to={to} className="landing-more" onNavigate={onNavigate}>
      {text}
    </Link>
  );
  const stops: readonly Stop[] = [
    {
      shot: "zoom",
      alt: t("The end of the sample at 25% zoom."),
      heading: t("A million rows"),
      text: t(
        "Change V1, and 100,001 formulas follow. Insert a row at the top, zoom out, scroll to the end. Go offline and reload.",
      ),
      link: page("/app", t("Open the demo")),
    },
    {
      shot: "sim",
      alt: t("Two replicas that have diverged, side by side."),
      heading: t("Sync by hand"),
      text: t(
        "Replicas and their server in one page. Hold a link back, edit on both sides, deliver the messages one by one. Or let five replicas run into faults from a seed.",
      ),
      link: page("/sim", t("Open the simulator")),
    },
    {
      shot: "review",
      alt: t("A draft in the sheet, and the review of its changes."),
      heading: t("Drafts to review"),
      text: t(
        "The AI Analyst edits a branch, never the sheet. The review shows each change, each conflict, and references that would break. Ten recorded runs need no key: press AI in the editor.",
      ),
      link: page("/app", t("Open the editor")),
    },
    {
      shot: "history",
      alt: t("An earlier revision of a shared sheet."),
      heading: t("Shared, with a history"),
      text: t(
        "Sign in with GitHub and send a link. Offline edits merge later, and every revision can be opened again.",
      ),
      link: page("/workbooks", t("Share a workbook")),
    },
  ];
  return (
    <section id="try" className="landing-section" aria-labelledby="try-heading">
      <h2 id="try-heading">{t("Try it")}</h2>
      {stops.map(({ shot, alt, heading, text, link }) => (
        <article key={shot} className="landing-stop">
          <Picture shot={shot} alt={alt} />
          <div>
            <h3>{heading}</h3>
            <p>{text}</p>
            {link}
          </div>
        </article>
      ))}
      <div className="landing-pair">
        <article>
          <h3>{t("Performance dashboard")}</h3>
          <p>{t("Every archived run, every known difference from LibreOffice.")}</p>
          {page("/benchmarks", t("Open the dashboard"))}
        </article>
        <article>
          <h3>{t("Source code")}</h3>
          <p>{t("Kernel, grid, server and tests in one repository.")}</p>
          <a href={REPOSITORY} {...OUTSIDE} className="landing-more">
            {t("View on GitHub")}
          </a>
        </article>
      </div>
    </section>
  );
}
