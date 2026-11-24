import { t } from "../i18n/index.ts";

export function Limits() {
  const limits: readonly (readonly [id: string, text: string])[] = [
    [
      "excel",
      t("Not Excel: no macros, charts, pivot tables, conditional formats, pictures or printing."),
    ],
    ["names", t("Formulas use commas and English function names.")],
    // StorageLimits in crates/server/src/store.rs.
    ["size", t("Shared workbooks hold up to 2 million cells.")],
    ["storage", t("Local workbooks are in this browser only, which may delete them.")],
    ["touch", t("No editing on phones and tablets.")],
    ["server", t("One server, which does not scale out.")],
    ["safari", t("Safari: Canvas 2D only, input methods untested.")],
    ["reader", t("Not yet tested with a screen reader by a person.")],
    ["model", t("The AI Analyst needs a language model on the server. No scores are published.")],
  ];
  return (
    <section id="limits" className="landing-section" aria-labelledby="limits-heading">
      <h2 id="limits-heading">{t("Known limits")}</h2>
      <ul className="landing-limits">
        {limits.map(([id, text]) => (
          <li key={id}>{text}</li>
        ))}
      </ul>
    </section>
  );
}
