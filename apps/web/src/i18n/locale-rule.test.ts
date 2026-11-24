import { describe, expect, it } from "vitest";
import { detectLocale, LOCALE_KEY, type Locale, type LocaleHost } from "./locale-rule.ts";

interface Visit {
  readonly search?: string;
  readonly stored?: string | null;
  readonly languages?: readonly string[];
}

function hostOf({ search = "", stored = null, languages = [] }: Visit): LocaleHost {
  return {
    location: { search },
    localStorage: { getItem: (key) => (key === LOCALE_KEY ? stored : "zh") },
    navigator: { languages },
  };
}

const VISITS: readonly (readonly [Visit, Locale])[] = [
  [{}, "en"],
  [{ languages: ["en-US"] }, "en"],
  [{ languages: ["zh-CN", "zh", "en"] }, "zh-CN"],
  [{ languages: ["zh-Hant-TW"] }, "zh-CN"],
  [{ languages: ["en-GB", "zh-CN"] }, "en"],
  [{ languages: ["fr-FR", "zh-CN", "en"] }, "zh-CN"],
  [{ languages: ["fr-FR", "de"] }, "en"],
  [{ languages: ["enm", "zha", "zh"] }, "zh-CN"],
  [{ stored: "zh-CN", languages: ["en-US"] }, "zh-CN"],
  [{ stored: "en", languages: ["zh-CN"] }, "en"],
  [{ stored: "nonsense", languages: ["zh-CN"] }, "zh-CN"],
  [{ search: "?lang=zh", stored: "en", languages: ["en"] }, "zh-CN"],
  [{ search: "?hooks&lang=en", stored: "zh-CN", languages: ["zh"] }, "en"],
  [{ search: "?lang=ZH-cn" }, "zh-CN"],
  [{ search: "?lang=fr", stored: "zh-CN" }, "zh-CN"],
  [{ search: "?lang=", languages: ["zh"] }, "zh-CN"],
  [{ search: "?language=zh" }, "en"],
];

describe("detectLocale", () => {
  it.each(VISITS)("decides for %o on %s", (visit, locale) => {
    expect(detectLocale(hostOf(visit), LOCALE_KEY)).toBe(locale);
  });

  it("does without storage", () => {
    const blocked: LocaleHost = {
      location: { search: "" },
      get localStorage(): Storage {
        throw new DOMException("denied", "SecurityError");
      },
      navigator: { languages: ["zh-CN"] },
    };
    expect(detectLocale(blocked, LOCALE_KEY)).toBe("zh-CN");
  });

  // The build puts the function's text on one line, into a script of its own.
  it("needs nothing but its parameters", () => {
    const text = detectLocale.toString().replace(/\s+/g, " ");
    expect(text).not.toMatch(/\/\/|\/\*|<\//);
    const alone = new Function(`return (${text})`)() as typeof detectLocale;
    for (const [visit, locale] of VISITS) {
      expect(alone(hostOf(visit), LOCALE_KEY), JSON.stringify(visit)).toBe(locale);
    }
  });
});
