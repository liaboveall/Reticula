import { t } from "../i18n/index.ts";

/** What is said of some parts of the editor to whoever points at them. */
export const HINTS = {
  workbooks: () => t("Manage local browser workbooks, import or export .rtcl / .csv"),
  cloud: () => t("Sign in, create, or share collaborative cloud workbooks"),
  ai: () => t("Open AI Analyst draft assistant and recorded demos"),
  history: () => t("Browse revision history and named versions"),
  seed: () => t("The sample is computed from this seed, not downloaded"),
  generation: () => t("Time the engine took to generate or open"),
  frame: () => t("Main-thread work per frame: input, receiving pages and issuing draw calls"),
} as const;
