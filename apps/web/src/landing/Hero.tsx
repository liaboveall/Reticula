import { t } from "../i18n/index.ts";
import { Link } from "./Link.tsx";
import { OUTSIDE, REPOSITORY } from "./links.ts";
import { Picture } from "./Picture.tsx";

/** What the page is about: its heading, and more of it. */
export const pitch = () =>
  [
    t("One million rows in a browser tab, edited together, online or offline."),
    t(
      "A local-first spreadsheet whose kernel (Rust in WebAssembly), grid (Canvas 2D and WebGPU) and sync protocol were written for it. Each number here says where it was measured.",
    ),
  ] as const;

export function Hero({ onNavigate }: { readonly onNavigate: (path: string) => void }) {
  const [heading, lead] = pitch();
  return (
    <section id="overview" className="landing-hero" aria-labelledby="overview-heading">
      <div className="landing-hero-text">
        <h1 id="overview-heading">{heading}</h1>
        <p className="landing-lead">{lead}</p>
        <div className="landing-actions">
          <Link to="/app" className="landing-cta" testId="open-demo" onNavigate={onNavigate}>
            {t("Open the demo")}
          </Link>
          <Link
            to="/app?doc=orders-10m&sample=10000000"
            className="landing-secondary"
            testId="open-sample-table"
            onNavigate={onNavigate}
          >
            {t("Open 10 million rows")}
          </Link>
          <Link to="/sim" className="landing-secondary" onNavigate={onNavigate}>
            {t("Open the simulator")}
          </Link>
          <a href={REPOSITORY} {...OUTSIDE} className="landing-secondary">
            {t("View on GitHub")}
          </a>
        </div>
        <p className="landing-note">{t("No sign-in, and nothing is uploaded.")}</p>
      </div>
      <figure>
        <Picture
          shot="sheet"
          first
          alt={t("A formula being written. The ranges it refers to are outlined in their colors.")}
        />
        <figcaption>
          {t("The demo: 1,000,000 rows and 100,001 formulas from a seed. Its words are Chinese.")}
        </figcaption>
      </figure>
    </section>
  );
}
