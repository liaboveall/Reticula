import { cloneElement, type ReactElement, type ReactNode, useSyncExternalStore } from "react";
import {
  fill,
  getLocale,
  type Locale,
  localeRevision,
  type Params,
  subscribeLocale,
  textOf,
} from "./index.ts";

/** The language, rendering again when it changes or when more of its texts arrive. */
export function useLocale(): Locale {
  useSyncExternalStore(subscribeLocale, localeRevision, localeRevision);
  return getLocale();
}

/**
 * A text with elements in it: `tx("Press <0>Enter</0>", { 0: <kbd /> })` puts
 * the text into the element, and `<0/>` stands for an element as it is.
 */
export function tx(
  source: string,
  slots: Readonly<Record<number, ReactElement>>,
  params?: Params,
): ReactNode[] {
  const text = textOf(source);
  const parts: ReactNode[] = [];
  let end = 0;
  for (const match of text.matchAll(/<(\d+)(?:\/>|>(.*?)<\/\1>)/g)) {
    const [whole, number, inside] = match;
    const slot = slots[Number(number)];
    parts.push(fill(text.slice(end, match.index), params));
    if (slot) {
      const children = inside === undefined ? [] : [fill(inside, params)];
      parts.push(cloneElement(slot, { key: match.index }, ...children));
    }
    end = match.index + whole.length;
  }
  parts.push(fill(text.slice(end), params));
  return parts;
}
