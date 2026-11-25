import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { expect, type Page, test } from "@playwright/test";
import { pressTab } from "./keyboard-helpers.ts";

// The front page: what it says, in both languages and on every width, where its
// links lead, and that it stays small.

const DIST_DIR = resolve(process.cwd(), process.env.RETICULA_E2E_DIST ?? "apps/web/dist");

async function open(page: Page, search = ""): Promise<void> {
  await page.goto(`/${search}`);
  await expect(page.getByTestId("landing-page")).toBeVisible();
}

/** Scrolls through the page, so that everything in it has been in view. */
async function scrollThrough(page: Page): Promise<void> {
  await page.evaluate(async () => {
    const frame = () => new Promise((next) => requestAnimationFrame(next));
    for (let top = 0; top < document.documentElement.scrollHeight; top += window.innerHeight / 2) {
      window.scrollTo(0, top);
      await frame();
    }
  });
}

test("the calls to action lead to what they name", async ({ page }) => {
  await open(page);
  const main = page.getByRole("main");
  for (const [link, path, heading] of [
    ["Open the simulator", "/sim", "Simulator"],
    ["Open the dashboard", "/benchmarks", "Performance & Differential Verification"],
    ["Share a workbook", "/workbooks", "Workbooks"],
  ] as const) {
    await main.getByRole("link", { name: link }).first().click();
    await expect(page).toHaveURL(new RegExp(`${path}$`));
    await expect(page.getByRole("heading", { level: 1 })).toContainText(heading);
    await page.goBack();
    await expect(page.getByTestId("landing-page")).toBeVisible();
  }

  for (const link of await main.getByRole("link", { name: "View on GitHub" }).all()) {
    await expect(link).toHaveAttribute("href", "https://github.com/liaboveall/Reticula");
    await expect(link).toHaveAttribute("target", "_blank");
  }
  await expect(page.getByRole("contentinfo").getByRole("link", { name: "Privacy" })).toBeVisible();

  await page.getByTestId("open-demo").click();
  await expect(page).toHaveURL(/\/app$/);
  await expect(page.getByTestId("engine-status")).toHaveText("ready", { timeout: 30_000 });
});

test("the language is changed in place, with the title, the description and the pictures", async ({
  page,
}) => {
  await open(page);
  const description = page.locator('meta[name="description"]');
  await expect(page).toHaveTitle("Reticula: a local-first spreadsheet");
  await expect(description).toHaveAttribute("content", /One million rows/);
  const simulator = page.locator("#try img").nth(1);
  await expect(simulator).toHaveAttribute("src", "/media/sim-en-light.webp");

  await page.getByTestId("landing-language").click();
  await expect(page.locator("html")).toHaveAttribute("lang", "zh-CN");
  await expect(page.getByRole("heading", { level: 1 })).toContainText("百万行");
  await expect(page).toHaveTitle("Reticula：本地优先的电子表格");
  await expect(description).toHaveAttribute("content", /百万行/);
  await expect(simulator).toHaveAttribute("src", "/media/sim-zh-light.webp");
  // Nothing is left in English but names, and what is written the same.
  const prose = await page.getByTestId("landing-page").locator("h1, h2, h3, p").allTextContents();
  expect(prose.join("\n").match(/\b[a-z]{3,} [a-z]{2,} [a-z]{3,}\b/g) ?? []).toEqual([]);

  await page.getByTestId("language-switch").getByRole("button", { name: "English" }).click();
  await expect(page.locator("html")).toHaveAttribute("lang", "en");
  await expect(page.getByRole("heading", { level: 1 })).toContainText("One million rows");
});

test("every number says where it comes from, or that there is none", async ({ page }) => {
  await open(page);
  const numbers = page.locator('[data-testid^="number-"]');
  expect(await numbers.count()).toBeGreaterThan(6);
  for (const number of await numbers.all()) {
    await expect(number.locator(".landing-value")).toHaveText(/^\d[\d.,]* (?:s|ms|µs|MiB)$/);
    const source = number.getByTestId("provenance");
    await expect(source).toContainText(/[0-9a-f]{7}/);
    await expect(source.locator("time")).toHaveText(/^20\d\d-\d\d-\d\d$/);
    await expect(source.getByRole("link", { name: "report" })).toHaveAttribute(
      "href",
      /\/bench\/results\/.+\.json$/,
    );
  }
  await expect(page.getByTestId("landing-page")).not.toContainText(/NaN|undefined|null/);
});

test("the pictures load, say what they show and hold their place", async ({ page }) => {
  await open(page);
  const images = page.getByRole("main").getByRole("img");
  const shift = () =>
    page.evaluate(() =>
      [...document.querySelectorAll("img")].map((image) =>
        Math.round(image.getBoundingClientRect().height),
      ),
    );
  const before = await shift();
  await scrollThrough(page);
  for (const image of await page.locator("img").all()) {
    await expect(image).toHaveAttribute("alt", /\S.{10}/);
    await expect(image).toHaveJSProperty("complete", true);
    expect(await image.evaluate((one: HTMLImageElement) => one.naturalWidth)).toBeGreaterThan(0);
  }
  expect(await shift()).toEqual(before);
  // The diagram is one too.
  await expect(images.and(page.locator("svg"))).toHaveAccessibleName(/browser.*server/);
});

test("pictures that cannot be loaded leave their place, with what they show in words", async ({
  page,
}) => {
  const sizes = () =>
    page.evaluate(() =>
      [...document.querySelectorAll("img")].map((image) => {
        const { width, height } = image.getBoundingClientRect();
        return [Math.round(width), Math.round(height)];
      }),
    );
  await open(page);
  await scrollThrough(page);
  const loaded = await sizes();
  await page.route("**/media/**", (route) => route.abort());
  await open(page);
  await scrollThrough(page);
  for (const image of await page.locator("img").all()) {
    await expect(image).toHaveJSProperty("complete", true);
    await expect(image).toHaveJSProperty("naturalWidth", 0);
    await expect(image).toHaveAccessibleName(/\S.{10}/);
  }
  expect(await sizes()).toEqual(loaded);
});

test("a dark theme has pictures of its own", async ({ page }) => {
  await page.emulateMedia({ colorScheme: "dark" });
  await open(page);
  await scrollThrough(page);
  const shown = await page
    .locator("img")
    .evaluateAll((images) => images.map((image) => (image as HTMLImageElement).currentSrc));
  expect(shown.length).toBeGreaterThan(3);
  expect(shown.filter((url) => !url.endsWith("-dark.webp"))).toEqual([]);
});

test("nothing is reported as a problem", async ({ page }) => {
  const problems: string[] = [];
  page.on("console", (message) => {
    if (message.type() === "error" || message.type() === "warning") {
      problems.push(message.text());
    }
  });
  page.on("pageerror", (error) => problems.push(error.message));
  page.on("requestfailed", (request) => problems.push(request.url()));
  page.on("response", (response) => {
    if (response.status() >= 400) {
      problems.push(`${response.status()} ${response.url()}`);
    }
  });
  await open(page);
  await scrollThrough(page);
  await page.waitForLoadState("networkidle");
  expect(problems).toEqual([]);
});

for (const width of [360, 768, 1280]) {
  for (const language of ["en", "zh"]) {
    test(`fits into ${width} pixels in ${language}`, async ({ page }) => {
      await page.setViewportSize({ width, height: 800 });
      await open(page, `?lang=${language}`);
      await scrollThrough(page);
      const wider = await page.evaluate(() => {
        const scroller = document.querySelector(".landing-tabs");
        return [...document.querySelectorAll("body *")]
          .filter((element) => !scroller?.contains(element) && !element.closest(".skip-button"))
          .filter((element) => element.getBoundingClientRect().right > window.innerWidth + 0.5)
          .map((element) => `${element.tagName}.${element.getAttribute("class")}`);
      });
      expect(wider).toEqual([]);
      expect(
        await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth),
      ).toBeLessThanOrEqual(0);
    });
  }
}

test("has landmarks, and headings in order", async ({ page }) => {
  await open(page);
  for (const landmark of ["banner", "main", "contentinfo"] as const) {
    await expect(page.getByRole(landmark)).toHaveCount(1);
  }
  await expect(page.getByRole("navigation", { name: "Pages" })).toHaveCount(1);
  await expect(page.getByRole("navigation", { name: "Sections" })).toHaveCount(1);
  const levels = await page
    .getByRole("main")
    .locator("h1, h2, h3, h4")
    .evaluateAll((headings) => headings.map((heading) => Number(heading.tagName.slice(1))));
  expect(levels[0]).toBe(1);
  expect(levels.filter((level) => level === 1).length).toBe(1);
  for (const [index, level] of levels.slice(1).entries()) {
    expect(level - (levels[index] ?? 0), `heading ${index + 2}`).toBeLessThanOrEqual(1);
  }
  for (const section of await page.getByRole("main").locator("section").all()) {
    await expect(section).toHaveAccessibleName(/\S/);
  }
});

test("the tabs along the bottom go through the sections", async ({ page, browserName }) => {
  await open(page);
  const tabs = page.getByTestId("landing-tabs").getByRole("link");
  await expect(tabs.first()).toHaveAttribute("aria-current", "true");
  const documents = await page.evaluate(() => window.history.length);

  const numbers = tabs.filter({ hasText: "Numbers" });
  await numbers.focus();
  await page.keyboard.press("Enter");
  await expect(numbers).toHaveAttribute("aria-current", "true");
  await expect(page.getByRole("heading", { level: 2, name: "Numbers" })).toBeInViewport();
  await expect(page.getByRole("heading", { level: 2, name: "Numbers" })).toBeFocused();
  await expect(page).toHaveURL(/#numbers$/);
  expect(await page.evaluate(() => window.history.length)).toBe(documents);

  // The keyboard goes on from there, and what it gets to is not behind the bar.
  // WebKit on Windows stops at no link.
  if (!(browserName === "webkit" && process.platform === "win32")) {
    await pressTab(page);
    const focused = page.locator(":focus");
    await expect(focused).toHaveAccessibleName(/Commit/);
    const link = await focused.boundingBox();
    const bar = await page.getByTestId("landing-tabs").boundingBox();
    expect((link?.y ?? 0) + (link?.height ?? 0)).toBeLessThanOrEqual(bar?.y ?? 0);
  }

  // Scrolling there says so too.
  await page.locator("#checks").evaluate((section) => section.scrollIntoView());
  await expect(tabs.filter({ hasText: "Checks" })).toHaveAttribute("aria-current", "true");

  // The last one may be too short to get far up.
  await page.evaluate(() => window.scrollTo(0, document.documentElement.scrollHeight));
  await expect(tabs.last()).toHaveAttribute("aria-current", "true");

  // An address that names a section opens at it.
  await page.goto("about:blank");
  await page.goto("/#limits");
  await expect(page.getByRole("heading", { name: "Known limits" })).toBeInViewport();
});

test("loads no engine and no editor, and pictures as they come into view", async ({ page }) => {
  // Such a device does not get the editor ahead of time.
  await page.addInitScript(() => {
    const matchMedia = window.matchMedia.bind(window);
    window.matchMedia = (query) =>
      matchMedia(query === "(pointer: fine)" ? "(width < 0px)" : query);
  });
  const paths: string[] = [];
  page.on("request", (request) => paths.push(new URL(request.url()).pathname));
  await open(page);
  await page.waitForLoadState("networkidle");
  expect(paths.filter((path) => path.endsWith(".wasm"))).toEqual([]);
  expect(paths.filter((path) => /\/(?:EditorPage|worker|AiPanel|SimLabPage)-/.test(path))).toEqual(
    [],
  );
  expect(paths).toContain("/media/sheet-light.webp");
  const when = await page
    .locator("img")
    .evaluateAll((images) => images.map((image) => (image as HTMLImageElement).loading));
  expect(when).toEqual(when.map((_, index) => (index === 0 ? "eager" : "lazy")));
});

test("says in its head what it is, for those who do not run it", async ({ page }) => {
  const html = await readFile(resolve(DIST_DIR, "index.html"), "utf8");
  for (const tag of ["og:title", "og:description", "og:image", "twitter:card", "description"]) {
    expect(html, tag).toMatch(new RegExp(`="${tag}"\\s+content="\\S`));
  }
  expect(html.match(/name="theme-color"/g)?.length).toBe(2);
  for (const path of [
    /property="og:image" content="([^"]+)"/.exec(html)?.[1],
    /rel="icon" href="([^"]+)"/.exec(html)?.[1],
  ]) {
    const response = await page.request.get(path ?? "missing");
    expect(response.ok(), path).toBe(true);
    expect(response.headers()["content-type"], path).toMatch(/^image\//);
  }
});
