import type { Locale } from "./locale-rule.ts";

/** Every language under its own name, which is found in any of them. */
export const LANGUAGES: readonly (readonly [Locale, string])[] = [
  ["en", "English"],
  ["zh-CN", "简体中文"],
];
