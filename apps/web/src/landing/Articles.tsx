import { getLocale, t } from "../i18n/index.ts";
import type { Locale } from "../i18n/locale-rule.ts";
import { OUTSIDE } from "./links.ts";

export interface Article {
  readonly url: string;
  readonly date: string;
  /** By the languages it has been published in. */
  readonly title: Readonly<Partial<Record<Locale, string>>>;
}

export const ARTICLES: readonly Article[] = [];

export function Articles({ articles }: { readonly articles: readonly Article[] }) {
  const inLanguage = articles.flatMap(({ url, date, title }) => {
    const text = title[getLocale()] ?? title.en;
    return text === undefined ? [] : [{ url, date, text }];
  });
  if (inLanguage.length === 0) {
    return null;
  }
  return (
    <section id="articles" className="landing-section" aria-labelledby="articles-heading">
      <h2 id="articles-heading">{t("Articles")}</h2>
      <ul className="landing-articles">
        {inLanguage.map(({ url, date, text }) => (
          <li key={url}>
            <a href={url} {...OUTSIDE}>
              {text}
            </a>{" "}
            <time dateTime={date}>{date}</time>
          </li>
        ))}
      </ul>
    </section>
  );
}
