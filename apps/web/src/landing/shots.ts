/** A picture of the product, which `tests/media/capture.mjs` takes. */
export interface Shot {
  /** Its size on the page. */
  readonly width: number;
  readonly height: number;
  /** Pixels of the file for each of them. */
  readonly scale: number;
  /** Whether it has words of the interface in it, and so is there in either language. */
  readonly words: boolean;
}

export const SHOTS = {
  sheet: { width: 1200, height: 508, scale: 2, words: false },
  zoom: { width: 738, height: 465, scale: 1, words: false },
  sim: { width: 992, height: 441, scale: 1, words: true },
  review: { width: 1000, height: 541, scale: 1, words: true },
  history: { width: 1000, height: 581, scale: 1, words: true },
} as const satisfies Readonly<Record<string, Shot>>;

export type ShotName = keyof typeof SHOTS;

/** The address of a picture. */
export function shotUrl(name: ShotName, language: "en" | "zh", theme: "light" | "dark"): string {
  const parts = SHOTS[name].words ? [name, language, theme] : [name, theme];
  return `/media/${parts.join("-")}.webp`;
}
