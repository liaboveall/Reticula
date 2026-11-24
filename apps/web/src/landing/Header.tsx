import { getLocale, setLocale, t } from "../i18n/index.ts";
import { Link } from "./Link.tsx";
import { OUTSIDE, REPOSITORY } from "./links.ts";

const BRAND = "Reticula";
const GITHUB = "GitHub";

/** The bar at the top, which looks like the editor's. */
export function Header({ onNavigate }: { readonly onNavigate: (path: string) => void }) {
  const [other, name] =
    getLocale() === "en" ? (["zh-CN", "中文"] as const) : (["en", "English"] as const);
  return (
    <header className="landing-header">
      <span className="landing-brand">{BRAND}</span>
      <nav className="landing-nav" aria-label={t("Pages")}>
        <Link to="/sim" className="landing-link" onNavigate={onNavigate}>
          {t("Sync Simulator")}
        </Link>
        <Link
          to="/benchmarks"
          className="landing-link"
          testId="nav-benchmarks"
          onNavigate={onNavigate}
        >
          {t("Benchmarks|page")}
        </Link>
        <Link to="/workbooks" className="landing-link" onNavigate={onNavigate}>
          {t("Workbooks")}
        </Link>
        <a href={REPOSITORY} {...OUTSIDE} className="landing-link">
          {GITHUB}
        </a>
      </nav>
      <button
        type="button"
        className="landing-link landing-language"
        data-testid="landing-language"
        lang={other}
        onClick={() => void setLocale(other)}
      >
        {name}
      </button>
      <Link to="/app" className="landing-cta landing-cta-small" onNavigate={onNavigate}>
        {t("Open the demo")}
      </Link>
    </header>
  );
}
