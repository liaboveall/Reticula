import { BENCHMARKS } from "../generated/benchmarks.ts";
import { t } from "../i18n/index.ts";
import { useTouchMode } from "./touch-mode.ts";

export interface MobileFallbackProps {
  readonly onNavigate: (path: string) => void;
  readonly onOpenAnyway: () => void;
}

/** What a narrow screen shows in place of the editor. */
export default function MobileFallback({ onNavigate, onOpenAnyway }: MobileFallbackProps) {
  const touchMode = useTouchMode();
  return (
    <div className="mobile-fallback" data-testid="mobile-fallback">
      <header className="mobile-header">
        <a
          href="/"
          className="mobile-brand"
          onClick={(e) => {
            e.preventDefault();
            onNavigate("/");
          }}
        >
          ← Reticula
        </a>
      </header>
      <main className="mobile-body">
        <h1>{t("Desktop Canvas Editor")}</h1>
        {touchMode ? (
          <div className="mobile-open">
            <button
              type="button"
              className="primary-btn"
              data-testid="open-read-only"
              aria-describedby="open-read-only-note"
              onClick={onOpenAnyway}
            >
              {t("Open read-only")}
            </button>
            <p id="open-read-only-note" className="mobile-notice">
              {t(
                "On a touch screen you can scroll, zoom and look into cells. Connect a mouse or a trackpad to edit them.",
              )}
            </p>
          </div>
        ) : (
          <p className="mobile-notice">
            {t(
              "Reticula's desktop canvas editor requires a keyboard and pointer — please open this link on desktop.",
            )}
          </p>
        )}

        <svg
          viewBox="0 0 360 176"
          className="mobile-preview"
          role="img"
          aria-label={t(
            "Preview of Reticula 1,000,000-row spreadsheet and live formula recalculation",
          )}
        >
          <rect x="4" y="4" width="352" height="168" rx="8" className="preview-frame" />
          <rect x="4" y="4" width="352" height="28" rx="8" className="preview-bar" />
          <text x="16" y="22" className="preview-title">
            {t("Reticula · 1,000,000 rows × 22 cols · fx =SUM(M2:M100001)")}
          </text>
          <line x1="4" y1="32" x2="356" y2="32" className="preview-line" />
          <text x="16" y="54" className="preview-cell">
            {t("U1 Promo Rate")}
          </text>
          <rect x="164" y="39" width="74" height="22" rx="3" className="preview-focus" />
          <text x="228" y="54" textAnchor="end" className="preview-num">
            5.00%
          </text>
          <text x="16" y="80" className="preview-cell">
            {t("U2 100k Net Total")}
          </text>
          <text x="228" y="80" textAnchor="end" className="preview-num">
            298,412,904.18
          </text>
          <line x1="12" y1="94" x2="348" y2="94" className="preview-line" />
          <text x="16" y="116" className="preview-muted">
            M2..M100001: =ROUND(J2*K2*(1-L2-$V$1), 2)
          </text>
          <text x="16" y="152" className="preview-status">
            {t("recalculated 100,001 cells in {ms} ms", { ms: BENCHMARKS.b4Incremental100kP50Ms })}
          </text>
        </svg>

        <div className="mobile-metrics">
          <div>
            <strong>{t("{ms} ms", { ms: BENCHMARKS.b4Incremental100kP50Ms })}</strong>
            <span>{t("100k-formula incremental recalc (p50)")}</span>
          </div>
          <div>
            <strong>{t("{us} µs", { us: BENCHMARKS.b7aInsertTopMeanUs })}</strong>
            <span>{t("Top-row insert on 1M-row sheet")}</span>
          </div>
          <div>
            <strong>{t("{ms} ms", { ms: BENCHMARKS.b2Sum1mP50Ms })}</strong>
            <span>{t("1,000,000-row SUM scan (p50)")}</span>
          </div>
        </div>

        {!touchMode && (
          <button
            type="button"
            className="mobile-anyway"
            data-testid="open-anyway"
            onClick={onOpenAnyway}
          >
            {t("Open editor anyway")}
          </button>
        )}
      </main>
    </div>
  );
}
