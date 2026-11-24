import { t } from "../i18n/index.ts";
import { tx } from "../i18n/react.ts";
import { LanguageSwitch } from "../pages/LanguageSwitch.tsx";
import { Link } from "./Link.tsx";
import { OUTSIDE, REPOSITORY, sourceUrl } from "./links.ts";

const GITHUB = "GitHub";

export function Footer({ onNavigate }: { readonly onNavigate: (path: string) => void }) {
  return (
    <footer className="landing-footer">
      <p>
        {tx("Licensed under <0>MIT</0> or <1>Apache-2.0</1>, at your option.", {
          0: <a href={sourceUrl("LICENSE-MIT")} {...OUTSIDE} />,
          1: <a href={sourceUrl("LICENSE-APACHE")} {...OUTSIDE} />,
        })}
      </p>
      <p>
        <Link to="/privacy" onNavigate={onNavigate}>
          {t("Privacy")}
        </Link>
        <a href={REPOSITORY} {...OUTSIDE}>
          {GITHUB}
        </a>
      </p>
      <LanguageSwitch />
    </footer>
  );
}
