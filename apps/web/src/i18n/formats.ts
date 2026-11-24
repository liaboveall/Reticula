/** Formats no first screen has, which is why they are not in `index.ts`. */

import { digits, intlTag, numberFormat } from "./index.ts";

/** A size in bytes, with its unit, which is the same in both languages. */
export function fmtBytes(bytes: number): string {
  const steps = ["B", "KB", "MB", "GB"];
  let value = Number.isFinite(bytes) ? Math.max(0, bytes) : 0;
  let step = 0;
  while (value >= 1024 && step < steps.length - 1) {
    value /= 1024;
    step++;
  }
  const format = numberFormat({ useGrouping: false, ...digits(Math.min(step, 2)) });
  return `${format.format(value)} ${steps[step]}`;
}

const DATE_AND_TIME: Intl.DateTimeFormatOptions = {
  year: "numeric",
  month: "numeric",
  day: "numeric",
  hour: "numeric",
  minute: "numeric",
  second: "numeric",
};

export function fmtDateTime(
  date: Date | number,
  options: Intl.DateTimeFormatOptions = DATE_AND_TIME,
): string {
  return new Intl.DateTimeFormat(intlTag(), options).format(date);
}
