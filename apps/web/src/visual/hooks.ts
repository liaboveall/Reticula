import type { RendererKind } from "@reticula/grid/internals";
import type { Comparison } from "./compare.ts";
import type { CaseName, Mutation } from "./names.ts";

export interface CaseOptions {
  /** Device pixels per CSS pixel; by default, those of the golden image. */
  readonly scale?: number;
  /**
   * Which theme to draw with; by default, that of the golden image. The page
   * must be emulating it: the theme is read from the style sheet.
   */
  readonly dark?: boolean;
  /** The generated font, or the one the product is drawn with. */
  readonly font?: "test" | "product";
}

export interface CompareOptions extends CaseOptions {
  readonly left?: RendererKind;
  readonly right?: RendererKind;
  /** Put into the right picture only. */
  readonly mutation?: Mutation;
}

export interface Picture {
  readonly width: number;
  readonly height: number;
  /** Base64 of the pixels, four bytes each. */
  readonly rgba: string;
}

export interface Report extends Comparison {
  readonly name: CaseName;
  readonly scale: number;
  readonly dark: boolean;
  readonly font: "test" | "product";
  readonly left: RendererKind;
  readonly right: RendererKind;
  /** Pixels that text changed although no text run says it draws there. */
  readonly strayInk: number;
  readonly firstStray: { readonly x: number; readonly y: number } | null;
  readonly milliseconds: number;
  /** Base64 of a PNG around the worst pixel (left, right, difference), if the comparison failed. */
  readonly diff: string | null;
}

export interface AdapterReport {
  /** Whether this build has a WebGPU renderer to compare. */
  readonly backend: boolean;
  /** Whether the browser offers an adapter, and which. */
  readonly available: boolean;
  readonly vendor: string;
  readonly architecture: string;
  readonly description: string;
  readonly fallback: boolean;
}

/** A window onto the visual regression page for the tests that drive it. */
export interface VisualHooks {
  /** Makes the generated font available; the page does not ship it. */
  loadFont(base64: string): Promise<void>;
  cases(): readonly CaseName[];
  /** Every character a case draws. */
  characters(name: CaseName): string;
  adapter(): Promise<AdapterReport>;
  pixels(name: CaseName, renderer: RendererKind, options?: CaseOptions): Promise<Picture>;
  compare(name: CaseName, options?: CompareOptions): Promise<Report>;
  /** Compares two pictures, one of which has a mistake. */
  mutate(name: CaseName, mutation: Mutation, options?: CompareOptions): Promise<Report>;
}

declare global {
  interface Window {
    __visual?: VisualHooks;
  }
}
