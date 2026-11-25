// Rows and columns with sizes of their own, and whole runs of them. Nothing here
// needs a browser.

/** Rows or columns with a size of their own, in order. */
export type CustomSizes = readonly (readonly [number, number])[];

/** Some rows or columns that follow each other, half-open. */
export interface Interval {
  readonly first: number;
  readonly end: number;
}

/** The same rows or columns, each of them once and in order. */
export function mergeIntervals(intervals: readonly Interval[]): Interval[] {
  const merged: { first: number; end: number }[] = [];
  for (const { first, end } of [...intervals].sort((a, b) => a.first - b.first)) {
    const last = merged.at(-1);
    if (first >= end) {
      continue;
    }
    if (last && first <= last.end) {
      last.end = Math.max(last.end, end);
    } else {
      merged.push({ first, end });
    }
  }
  return merged;
}

export function sizeIn(sizes: CustomSizes, index: number, standard: number): number {
  return sizes.find(([other]) => other === index)?.[1] ?? standard;
}

/** The sizes once some rows or columns all have one; the standard size is none of their own. */
export function withSizes(
  sizes: CustomSizes,
  intervals: readonly Interval[],
  px: number,
  standard: number,
): CustomSizes {
  const runs = mergeIntervals(intervals);
  const isResized = (index: number) => runs.some(({ first, end }) => index >= first && index < end);
  const result = sizes
    .filter(([index]) => !isResized(index))
    .map(([index, size]) => [index, size] as const);
  if (px !== standard) {
    for (const { first, end } of runs) {
      for (let index = first; index < end; index++) {
        result.push([index, px]);
      }
    }
    result.sort((a, b) => a[0] - b[0]);
  }
  return result;
}
