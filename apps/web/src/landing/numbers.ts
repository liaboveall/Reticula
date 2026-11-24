import { fmtNumber, t } from "../i18n/index.ts";

/** A measured number in words, or that there is none. */
export function measured(value: number | null, show: (value: number) => string): string {
  return value === null ? t("not available") : show(value);
}

/** Three significant digits, and no less than whole numbers. */
function digits(value: number): string {
  return fmtNumber(value, value < 10 ? 2 : value < 100 ? 1 : 0);
}

export const ms = (value: number) => t("{ms} ms", { ms: digits(value) });
export const us = (value: number) => t("{us} µs", { us: fmtNumber(value, 1) });
export const seconds = (milliseconds: number) =>
  t("{s} s", { s: fmtNumber(milliseconds / 1000, 2) });
export const mib = (value: number) => `${fmtNumber(value, 0)} MiB`;
export const percent = (share: number) => `${fmtNumber(share * 100, 2)}%`;
