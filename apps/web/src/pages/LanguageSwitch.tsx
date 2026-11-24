import { setLocale, t } from "../i18n/index.ts";
import { LANGUAGES } from "../i18n/languages.ts";
import { useLocale } from "../i18n/react.ts";

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
