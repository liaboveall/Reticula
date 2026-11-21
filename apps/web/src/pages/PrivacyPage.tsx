import "./pages.css";

export interface PrivacyPageProps {
  readonly onNavigate: (path: string) => void;
}

export default function PrivacyPage({ onNavigate }: PrivacyPageProps) {
  return (
    <div className="page-shell" data-testid="privacy-page">
      <nav className="page-nav">
        <a
          href="/"
          className="page-brand"
          onClick={(e) => {
            e.preventDefault();
            onNavigate("/");
          }}
        >
          Reticula
        </a>
        <div className="page-nav-links">
          <a
            href="/app"
            className="toolbar-btn"
            onClick={(e) => {
              e.preventDefault();
              onNavigate("/app");
            }}
          >
            Editor
          </a>
          <a
            href="/workbooks"
            className="toolbar-btn"
            onClick={(e) => {
              e.preventDefault();
              onNavigate("/workbooks");
            }}
          >
            Workbooks
          </a>
          <a
            href="/account"
            className="toolbar-btn"
            onClick={(e) => {
              e.preventDefault();
              onNavigate("/account");
            }}
          >
            Account
          </a>
        </div>
      </nav>

      <main className="page-main">
        <div className="page-card">
          <h1>Privacy &amp; Data Notice</h1>
          <p className="page-muted">
            Reticula is an independent personal project built as a local-first spreadsheet engine
            with optional collaborative cloud storage.
          </p>
          <h2>What Data Is Stored</h2>
          <ul>
            <li>
              <strong>Local mode (/app without Cloud):</strong> All spreadsheets stay strictly
              inside your browser&apos;s Origin Private File System (OPFS) or memory and never leave
              your device.
            </li>
            <li>
              <strong>Cloud collaboration (opt-in sign-in):</strong> When you sign in via GitHub,
              the server stores your GitHub numeric user ID, public login/display name, avatar URL,
              and the cloud workbooks, revision logs, and snapshots you create or share.
            </li>
          </ul>
          <h2>Cookies &amp; Tracking</h2>
          <p>
            No third-party analytics, ads, or tracking scripts are used. Cloud sign-in uses only
            strictly necessary first-party <code>HttpOnly</code> session cookies.
          </p>
          <h2>Who Can Access Cloud Workbooks</h2>
          <p>
            Workbooks default to <code>private</code> and are accessible only to explicit workbook
            members unless the owner creates an active link-view share token and enables{" "}
            <code>link_view</code>.
          </p>
          <h2>Deleting Your Data &amp; Backups</h2>
          <p>
            You can permanently erase your profile, login session, and all owned cloud workbooks at
            any time from the{" "}
            <a
              href="/account"
              onClick={(e) => {
                e.preventDefault();
                onNavigate("/account");
              }}
            >
              Account page
            </a>
            . Because this is a personal project with no commercial SLA guarantee, export important
            workbooks regularly as <code>.rtcl</code> snapshots.
          </p>
        </div>
      </main>
    </div>
  );
}
