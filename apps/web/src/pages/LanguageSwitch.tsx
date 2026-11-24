import { setLocale, t } from "../i18n/index.ts";
import type { Locale } from "../i18n/locale-rule.ts";
import { useLocale } from "../i18n/react.ts";

/** Every language under its own name, which is found in any of them. */
const LANGUAGES: readonly (readonly [Locale, string])[] = [
  ["en", "English"],
  ["zh-CN", "简体中文"],
];

export function LanguageSwitch() {
  const shown = useLocale();
  return (
    <fieldset className="page-language" data-testid="language-switch">
      <legend>{t("Language")}</legend>
      {LANGUAGES.map(([locale, name]) => (
        <button
          key={locale}
          type="button"
          lang={locale}
          className={shown === locale ? "page-tab active" : "page-tab"}
          aria-pressed={shown === locale}
          onClick={() => void setLocale(locale)}
        >
          {name}
        </button>
      ))}
    </fieldset>
  );
}
