import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

// The colors are read from the style sheets, as a browser would in either theme.

const WEB = fileURLToPath(new URL("..", import.meta.url));
const GRID = fileURLToPath(new URL("../../../../packages/grid/src", import.meta.url));

const read = (directory: string, file: string) => readFileSync(join(directory, file), "utf8");

function styleSheets(directory: string): string[] {
  return readdirSync(directory, { recursive: true, encoding: "utf8" })
    .filter((name) => name.endsWith(".css"))
    .map((name) => name.replaceAll("\\", "/"));
}

/** The custom properties a selector's rules give values to, outside of or inside the dark theme. */
function tokens(
  css: string,
  selector: string,
): { light: Map<string, string>; dark: Map<string, string> } {
  const dark = [...css.matchAll(/@media \(prefers-color-scheme: dark\) \{([\s\S]*?)\n\}/g)];
  const lightCss = dark.reduce((rest, [block]) => rest.replace(block, ""), css);
  const collect = (part: string) => {
    const found = new Map<string, string>();
    for (const [, body] of part.matchAll(
      new RegExp(`(?:^|\\n)\\s*${selector.replace(".", "\\.")} \\{([^}]*)\\}`, "g"),
    )) {
      for (const [, name, value] of (body ?? "").matchAll(/(--[\w-]+):\s*([^;]+);/g)) {
        found.set(name ?? "", (value ?? "").trim());
      }
    }
    return found;
  };
  const light = collect(lightCss);
  return { light, dark: new Map([...light, ...collect(dark.map(([, body]) => body).join("\n"))]) };
}

function luminance(color: string): number {
  const match = /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/.exec(color);
  if (!match) {
    throw new Error(`${color} is not a plain color`);
  }
  const [r = 0, g = 0, b = 0] = match.slice(1).map((part) => {
    const value = Number.parseInt(part, 16) / 255;
    return value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function contrast(one: string, other: string): number {
  const [lighter = 0, darker = 0] = [luminance(one), luminance(other)].sort((a, b) => b - a);
  return (lighter + 0.05) / (darker + 0.05);
}

const shell = tokens(read(WEB, "app.css"), ":root");
const ai = tokens(read(WEB, "ai/ai.css"), ":root");
const grid = tokens(read(GRID, "grid.css"), ".reticula-grid");

const SURFACES = ["--shell-background", "--shell-surface"];
const TEXT = 4.5;
const SIGN = 3;

/** What is seen on what, and how well it has to be. */
const PAIRS: readonly (readonly [string, readonly string[], number])[] = [
  ["--shell-text", [...SURFACES, "--shell-accent-soft", "--shell-danger-soft"], TEXT],
  ["--shell-text", ["--shell-warning-soft"], TEXT],
  ["--shell-muted", [...SURFACES, "--shell-accent-soft"], TEXT],
  ["--shell-accent", [...SURFACES, "--shell-accent-soft"], TEXT],
  ["--shell-danger", [...SURFACES, "--shell-danger-soft"], TEXT],
  ["--shell-success", SURFACES, TEXT],
  ["--shell-warning", [...SURFACES, "--shell-warning-soft"], TEXT],
  ["--shell-on-accent", ["--shell-accent"], TEXT],
  ["--ai-accent", [...SURFACES, "--ai-accent-soft"], TEXT],
  ["--ai-on-accent", ["--ai-accent"], TEXT],
  ["--shell-control", [...SURFACES, "--shell-accent-soft", "--shell-danger-soft"], SIGN],
  ["--shell-focus", [...SURFACES, "--shell-accent-soft", "--shell-danger-soft"], SIGN],
  ["--grid-scrollbar-thumb", ["--grid-scrollbar-track"], SIGN],
  ["--grid-scrollbar-thumb-active", ["--grid-scrollbar-track"], SIGN],
  ["--grid-selection", ["--grid-background"], SIGN],
  ["--grid-header-text", ["--grid-header-background", "--grid-header-selected"], TEXT],
  ["--grid-text", ["--grid-background"], TEXT],
  ["--grid-error-text", ["--grid-background"], TEXT],
];

describe.each(["light", "dark"] as const)("the colors of the %s theme", (theme) => {
  const colors = new Map([...shell[theme], ...ai[theme], ...grid[theme]]);
  const color = (name: string) => colors.get(name) ?? "missing";

  it.each(PAIRS)("tell %s from what it is on", (name, backgrounds, wanted) => {
    for (const background of backgrounds) {
      const ratio = contrast(color(name), color(background));
      expect(ratio, `${name} on ${background}: ${ratio.toFixed(2)}`).toBeGreaterThanOrEqual(wanted);
    }
  });
});

describe("custom properties", () => {
  // Set by scripts on the elements they are for.
  const SET_ELSEWHERE = new Set(["--editor-padding", "--ratio"]);
  const EXPERIMENTS = /^(?:bench|ime|visual|ui\/harness)\//;

  it("are the same ones in both themes", () => {
    for (const { light, dark } of [shell, ai, grid]) {
      expect([...dark.keys()].sort()).toEqual([...light.keys()].sort());
    }
  });

  it("are defined where they are used", () => {
    const sheets = [
      ...styleSheets(WEB)
        .filter((name) => !EXPERIMENTS.test(name))
        .map((name) => [name, read(WEB, name)] as const),
      ...styleSheets(GRID).map((name) => [name, read(GRID, name)] as const),
    ];
    const defined = new Set([
      ...shell.light.keys(),
      ...ai.light.keys(),
      ...grid.light.keys(),
      ...SET_ELSEWHERE,
    ]);
    const undefinedOnes = sheets.flatMap(([name, css]) =>
      [...css.matchAll(/var\((--[\w-]+)/g)]
        .filter(([, used]) => !defined.has(used ?? ""))
        .map(([, used]) => `${name}: ${used}`),
    );
    expect(undefinedOnes).toEqual([]);
  });
});
