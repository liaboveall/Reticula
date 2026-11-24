/** A picture of the grid that the visual regression tests know by name. */
export interface CaseInfo {
  /** Device pixels per CSS pixel of the golden image. */
  readonly scale: number;
  /** Whether the golden image is of the dark theme. */
  readonly dark: boolean;
}

/** Every device scale the two renderers are compared at. */
export const SCALES = [0.25, 0.5, 1, 1.25, 1.5, 2] as const;

// Kept apart from the cases themselves, which need a browser: the tests take
// their names from here.
export const CASES = {
  "plain-text": { scale: 1, dark: false },
  "small-text": { scale: 0.25, dark: true },
  numbers: { scale: 1.25, dark: false },
  alignment: { scale: 1.5, dark: true },
  han: { scale: 0.5, dark: false },
  hebrew: { scale: 2, dark: false },
  "combining-marks": { scale: 1.25, dark: true },
  "long-run": { scale: 1, dark: false },
  overhang: { scale: 1.5, dark: false },
  overflow: { scale: 1, dark: true },
  "overflow-across-pages": { scale: 1.25, dark: false },
  "overflow-scrolled-off": { scale: 1.5, dark: true },
  frozen: { scale: 1, dark: false },
  "frozen-overflow": { scale: 1.25, dark: true },
  "custom-sizes": { scale: 1.5, dark: false },
  borders: { scale: 1.25, dark: false },
  styles: { scale: 1, dark: true },
  selection: { scale: 1, dark: false },
  "multi-selection": { scale: 1.5, dark: true },
  "formula-references": { scale: 1.25, dark: true },
  dashes: { scale: 1, dark: false },
  "dashes-cut-off": { scale: 1.5, dark: false },
  filter: { scale: 1, dark: true },
  diff: { scale: 1.25, dark: false },
  collaborators: { scale: 1.5, dark: false },
  "header-labels": { scale: 0.25, dark: false },
  placeholders: { scale: 1, dark: true },
  "reorder-guide": { scale: 2, dark: true },
  "every-overlay": { scale: 2, dark: false },
} as const satisfies Record<string, CaseInfo>;

export type CaseName = keyof typeof CASES;

export const CASE_NAMES = Object.keys(CASES) as CaseName[];

/** A mistake put into one of two pictures, which comparing them must find. */
export const MUTATIONS = [
  "grid-lines",
  "selection-fill",
  "cell-background",
  "border",
  "dash",
  "text-run",
] as const;

export type Mutation = (typeof MUTATIONS)[number];
