import { t } from "../i18n/index.ts";
import { tx } from "../i18n/react.ts";
import { PageLink, PageNav } from "./PageNav.tsx";
import "./pages.css";

export interface PrivacyPageProps {
  readonly onNavigate: (path: string) => void;
}

export default function PrivacyPage({ onNavigate }: PrivacyPageProps) {
  return (
    <div className="page-shell" data-testid="privacy-page">
      <PageNav
        onNavigate={onNavigate}
        links={[
          ["/app", t("Editor")],
          ["/workbooks", t("Workbooks")],
          ["/account", t("Account")],
        ]}
      />

      <main className="page-main">
        <div className="page-card">
          <h1>{t("Privacy & Data Notice")}</h1>
          <p className="page-muted">
            {t(
              "Reticula is an independent personal project built as a local-first spreadsheet engine with optional collaborative cloud storage.",
            )}
          </p>
          <h2>{t("What Data Is Stored")}</h2>
          <ul>
            <li>
              {tx(
                "<0>Local mode (/app without Cloud):</0> All spreadsheets stay strictly inside your browser's Origin Private File System (OPFS) or memory and never leave your device.",
                { 0: <strong /> },
              )}
            </li>
            <li>
              {tx(
                "<0>Cloud collaboration (opt-in sign-in):</0> When you sign in via GitHub, the server stores your GitHub numeric user ID, public login/display name, avatar URL, and the cloud workbooks, revision logs, and snapshots you create or share.",
                { 0: <strong /> },
              )}
            </li>
          </ul>
          <h2>{t("Cookies & Tracking")}</h2>
          <p>
            {tx(
              "No third-party analytics, ads, or tracking scripts are used. Cloud sign-in uses only strictly necessary first-party <0>HttpOnly</0> session cookies.",
              { 0: <code /> },
            )}
          </p>
          <h2>{t("Who Can Access Cloud Workbooks")}</h2>
          <p>
            {tx(
              "Workbooks default to <0>private</0> and are accessible only to explicit workbook members unless the owner creates an active link-view share token and enables <0>link_view</0>.",
              { 0: <code /> },
            )}
          </p>
          <h2>{t("Deleting Your Data & Backups")}</h2>
          <p>
            {tx(
              "You can permanently erase your profile, login session, and all owned cloud workbooks at any time from the <0>Account page</0>. Because this is a personal project with no commercial SLA guarantee, export important workbooks regularly as <1>.rtcl</1> snapshots.",
              { 0: <PageLink to="/account" onNavigate={onNavigate} />, 1: <code /> },
            )}
          </p>
        </div>
      </main>
    </div>
  );
}
