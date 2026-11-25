import { readFile } from "node:fs/promises";
import { createServer, type Server } from "node:http";
import { extname, join, resolve } from "node:path";
import { expect, type Page, test } from "@playwright/test";
import { type AppRoute, normalizeAppRoute } from "../../apps/web/src/routes.ts";
import { listen } from "./listen.ts";

// How the app starts, moves between its pages and fails: what is downloaded
// when, and what is on screen when some of it cannot be.

const DIST_DIR = resolve(process.cwd(), process.env.RETICULA_E2E_DIST ?? "apps/web/dist");

/**
 * Scripts that may be fetched before the first screen without being part of
 * the editor's static closure, by the start of their names. Every one of them
 * competes with the engine for the network.
 */
const ALLOWED_BEFORE_FIRST_SCREEN: readonly string[] = [];

const MIME_TYPES: Readonly<Record<string, string>> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".wasm": "application/wasm",
  ".json": "application/json; charset=utf-8",
};

interface DistServer {
  readonly origin: string;
  /** The paths of all requests, in the order they came in. */
  readonly requests: readonly string[];
  /** Files under `/assets/` that answer 404, as after a deploy that replaced them. */
  missing: RegExp | null;
  /** Paths that are answered only after a while. */
  slow: RegExp | null;
  /** Called before a document is sent. */
  onDocument: () => void;
  hits(file: string): number;
  documents(): number;
  stop(): Promise<void>;
}

/**
 * Serves the build the way the real server does where it matters here: what is
 * under `/assets/` may be cached for good, and is not found if it is not there.
 */
async function startDistServer(): Promise<DistServer> {
  const requests: string[] = [];
  let documents = 0;
  const server: Server = createServer(async (request, response) => {
    const { pathname } = new URL(request.url ?? "/", "http://localhost");
    requests.push(pathname);
    const isAsset = pathname.startsWith("/assets/");
    if (!isAsset && extname(pathname) !== "" && pathname !== "/index.html") {
      response.writeHead(404).end();
      return;
    }
    const file = isAsset ? pathname.slice(1) : "index.html";
    if (!isAsset) {
      documents++;
      handle.onDocument();
    }
    if (handle.slow?.test(pathname)) {
      await new Promise((later) => setTimeout(later, isAsset ? 3_000 : 500));
    }
    try {
      if (isAsset && handle.missing?.test(pathname)) {
        throw new Error("replaced by a deploy");
      }
      const body = await readFile(join(DIST_DIR, file));
      response
        .writeHead(200, {
          "content-type": MIME_TYPES[extname(file)] ?? "application/octet-stream",
          "cache-control": isAsset ? "public, max-age=31536000, immutable" : "no-cache",
        })
        .end(body);
    } catch {
      response.writeHead(404, { "cache-control": "no-cache" }).end();
    }
  });
  const port = await listen(server);
  const handle: DistServer = {
    origin: `http://localhost:${port}`,
    requests,
    missing: null,
    slow: null,
    onDocument: () => {},
    hits: (file) => requests.filter((path) => path === `/assets/${file}`).length,
    documents: () => documents,
    stop: async () => {
      server.closeAllConnections();
      await new Promise<void>((closed) => server.close(() => closed()));
    },
  };
  return handle;
}

interface Build {
  /** Files under `/assets/` by what needs them, as the built document lists them. */
  readonly entry: readonly string[];
  readonly landing: readonly string[];
  readonly editor: readonly string[];
  readonly engine: readonly string[];
}

function parseSet(html: string, name: string): readonly string[] {
  const list = new RegExp(`"${name}":(\\[[^\\]]*\\])`).exec(html)?.[1];
  if (list === undefined) {
    throw new Error(`the document's inline script has no set "${name}"`);
  }
  return JSON.parse(list) as string[];
}

async function readBuild(): Promise<Build> {
  const html = await readFile(join(DIST_DIR, "index.html"), "utf8");
  const tags = html.matchAll(/<(?:script|link)\b[^>]*?\b(?:src|href)="\/assets\/([^"]+\.js)"/g);
  return {
    entry: [...tags].map((match) => match[1] ?? ""),
    landing: parseSet(html, "landing"),
    editor: parseSet(html, "app"),
    engine: parseSet(html, "engine"),
  };
}

const isScript = (name: string) => name.endsWith(".js");

/** Waits until everything on the first screen of a newly opened editor has its data. */
async function firstScreen(page: Page): Promise<void> {
  await page.waitForFunction(
    () => performance.getEntriesByName("reticula:first-screen").length > 0,
    null,
    { timeout: 20_000 },
  );
}

/** Makes the browser report a pointer, and whether its user wants to save data. */
async function emulateDevice(
  page: Page,
  device: { readonly finePointer: boolean; readonly saveData: boolean },
): Promise<void> {
  await page.addInitScript(({ finePointer, saveData }) => {
    const { matchMedia } = window;
    window.matchMedia = (query) =>
      query === "(pointer: fine)"
        ? Object.assign(new EventTarget(), {
            matches: finePointer,
            media: query,
            onchange: null,
            addListener: () => {},
            removeListener: () => {},
          })
        : matchMedia.call(window, query);
    Object.defineProperty(navigator, "connection", { value: { saveData }, configurable: true });
  }, device);
}

/** The names of the files under `/assets/` a page has asked for, with when it did. */
function assetRequests(page: Page): Promise<{ name: string; startTime: number }[]> {
  return page.evaluate(() =>
    performance
      .getEntriesByType("resource")
      .filter((entry) => new URL(entry.name).pathname.startsWith("/assets/"))
      .map((entry) => ({ name: entry.name.split("/").pop() ?? "", startTime: entry.startTime })),
  );
}

// A Service Worker would fetch every file once more for its cache.
test.use({ serviceWorkers: "block" });

let server: DistServer;
test.beforeEach(async () => {
  server = await startDistServer();
});
test.afterEach(async () => {
  await server.stop();
});

test("the document's first script preloads, and no style sheet comes before it", async () => {
  const html = await readFile(join(DIST_DIR, "index.html"), "utf8");
  const script = html.indexOf("<script");
  expect(html.slice(script)).toMatch(/^<script>\(\(p,x\)=>/);
  expect(html.indexOf("stylesheet")).toBeGreaterThan(script);
});

/**
 * What must have been downloaded exactly once. Playwright's WebKit keeps
 * nothing a page fetched for that page's workers, so there the worker's files
 * come a second time, preloaded or not.
 */
function countedOnce(browserName: string, build: Build, files: readonly string[]): string[] {
  return files.filter((name) => browserName !== "webkit" || !build.engine.includes(name));
}

test("the editor's first screen needs its static closure, all of it, once", async ({
  page,
  browserName,
}) => {
  const build = await readBuild();
  await page.goto(`${server.origin}/app`);
  await firstScreen(page);

  // The page and its worker have a timeline each, on clocks of their own.
  const until = await page.evaluate(() => {
    const [mark] = performance.getEntriesByName("reticula:first-screen");
    return performance.timeOrigin + (mark?.startTime ?? 0);
  });
  const requestsBefore = (until: number) =>
    performance
      .getEntriesByType("resource")
      .filter((entry) => performance.timeOrigin + entry.startTime < until)
      .map((entry) => entry.name.split("/").pop() ?? "");
  const workers = page.workers().map((worker) => worker.evaluate(requestsBefore, until));
  const early = [
    await page.evaluate(requestsBefore, until),
    ...(await Promise.all(workers)),
  ].flat();
  expect(early.filter(isScript)).not.toEqual([]);
  const expected = [...build.entry, ...build.editor, ...build.engine].filter(isScript);
  const beyond = early.filter(
    (name) =>
      isScript(name) &&
      !expected.includes(name) &&
      !ALLOWED_BEFORE_FIRST_SCREEN.some((prefix) => name.startsWith(prefix)),
  );
  expect(beyond).toEqual([]);

  // What is preloaded is used, and using it does not download it again.
  const files = [...build.entry, ...build.editor, ...build.engine];
  const hits = countedOnce(browserName, build, files).map((name) => [name, server.hits(name)]);
  expect(hits).toEqual(hits.map(([name]) => [name, 1]));
  expect(files.filter((name) => server.hits(name) === 0)).toEqual([]);
});

test("the inline script decides between the pages as the app does", async ({ page }) => {
  const html = await readFile(join(DIST_DIR, "index.html"), "utf8");
  const engine = parseSet(html, "engine");
  // Without the entry nothing but the inline script adds anything to the head.
  await page.route(/\/assets\/index-[^/]*\.js$/, (route) => route.abort());
  const paths = [
    "/",
    "/app",
    "/app/",
    "/app/x",
    "/apps",
    "/join",
    "/join/",
    "/join/some-token",
    "/workbooks",
    "/workbooks/x",
    "/account",
    "/privacy/",
    "/sim",
    "/benchmarks",
    "/landing",
    "/engine",
    "/constructor",
  ];
  for (const path of paths) {
    const route: AppRoute = normalizeAppRoute(path);
    await page.goto(`${server.origin}${path}`);
    const preloaded = await page.evaluate(() =>
      [
        ...document.querySelectorAll<HTMLLinkElement>(
          'link[rel="modulepreload"], link[rel="preload"]',
        ),
      ].map((link) => link.href.split("/").pop()),
    );
    expect(preloaded, path).toEqual([...parseSet(html, route), ...(route === "app" ? engine : [])]);
  }
});

for (const [why, device] of [
  ["data is to be saved", { finePointer: true, saveData: true }],
  ["the pointer is coarse", { finePointer: false, saveData: false }],
] as const) {
  test(`the landing page leaves the editor and the engine alone when ${why}`, async ({ page }) => {
    const build = await readBuild();
    await emulateDevice(page, device);
    await page.goto(`${server.origin}/`);
    await expect(page.getByTestId("landing-page")).toBeVisible();
    await page.waitForLoadState("load");
    await page.waitForTimeout(1_500);

    const scripts = (await assetRequests(page)).map(({ name }) => name).filter(isScript);
    const expected = [...build.entry, ...build.landing];
    expect(scripts.filter((name) => !expected.includes(name))).toEqual([]);
    expect(server.requests.filter((path) => path.endsWith(".wasm"))).toEqual([]);
  });
}

test("an idle landing page fetches the editor's code, and the engine once the demo is reached for", async ({
  page,
  browserName,
}) => {
  const build = await readBuild();
  await emulateDevice(page, { finePointer: true, saveData: false });
  await page.goto(`${server.origin}/`);
  const demo = page.getByTestId("open-demo");
  await expect(demo).toBeVisible();

  const editor = build.editor.filter((name) => !build.landing.includes(name));
  await expect.poll(() => editor.map((name) => server.hits(name))).toEqual(editor.map(() => 1));
  // Not as a part of loading the page.
  const { loaded, requests } = {
    loaded: await page.evaluate(() => {
      const [navigation] = performance.getEntriesByType("navigation");
      return (navigation as PerformanceNavigationTiming).loadEventStart;
    }),
    requests: await assetRequests(page),
  };
  for (const { name, startTime } of requests) {
    if (editor.includes(name)) {
      expect(startTime, name).toBeGreaterThanOrEqual(loaded);
    }
  }
  expect(build.engine.map((name) => server.hits(name))).toEqual(build.engine.map(() => 0));

  await demo.hover();
  await expect
    .poll(() => build.engine.map((name) => server.hits(name)))
    .toEqual(build.engine.map(() => 1));

  await demo.click();
  await firstScreen(page);
  const files = [...build.entry, ...build.landing, ...build.editor, ...build.engine];
  const hits = countedOnce(browserName, build, files).map((name) => [name, server.hits(name)]);
  expect(hits).toEqual(hits.map(([name]) => [name, 1]));
});

test("a page of text asks for nothing but its own code", async ({ page }) => {
  const build = await readBuild();
  await page.goto(`${server.origin}/privacy`);
  await expect(page.getByTestId("privacy-page")).toBeVisible();
  await page.waitForLoadState("load");
  await page.waitForTimeout(500);
  const scripts = (await assetRequests(page)).map(({ name }) => name).filter(isScript);
  expect(
    scripts.filter((name) => !build.entry.includes(name) && !/^(?:PrivacyPage|page)-/.test(name)),
  ).toEqual([]);
  expect(server.requests.filter((path) => path.endsWith(".wasm"))).toEqual([]);
});

test("pages are changed without loading a document, forwards and back", async ({ page }) => {
  await page.goto(`${server.origin}/?hooks`);
  await expect(page.getByTestId("landing-page")).toBeVisible();

  await page.getByTestId("open-demo").click();
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await expect(page).toHaveURL(`${server.origin}/app?hooks=`);

  await page.goBack();
  await expect(page.getByTestId("landing-page")).toBeVisible();
  await expect(page.getByTestId("grid")).toHaveCount(0);

  await page.getByTestId("nav-benchmarks").click();
  await expect(page.getByTestId("benchmarks-page")).toBeVisible();
  await expect(page).toHaveURL(`${server.origin}/benchmarks`);
  await page.goBack();
  await expect(page.getByTestId("landing-page")).toBeVisible();
  await page.goForward();
  await expect(page.getByTestId("benchmarks-page")).toBeVisible();

  expect(server.documents()).toBe(1);
  await expect(page.locator(".route-pending")).toHaveCount(0);
});

test("the last of two navigations in a row wins", async ({ page }) => {
  await page.goto(`${server.origin}/`);
  await expect(page.getByTestId("landing-page")).toBeVisible();
  // The first page's code is slower to come than the second's.
  await page.route(/\/assets\/WorkbooksPage-/, async (route) => {
    await new Promise((later) => setTimeout(later, 600));
    await route.continue();
  });
  await page.getByRole("link", { name: "Workbooks" }).click();
  await expect(page.locator(".route-pending")).toBeVisible();
  await page.getByTestId("nav-benchmarks").click();
  await expect(page.getByTestId("benchmarks-page")).toBeVisible();
  await page.waitForTimeout(900);
  await expect(page.getByTestId("benchmarks-page")).toBeVisible();
  await expect(page).toHaveURL(`${server.origin}/benchmarks`);
});

test("a page whose code is gone after a deploy is loaded afresh, and only then given up", async ({
  page,
}) => {
  // Nothing is fetched ahead of time, so the deploy comes before the editor's code.
  await emulateDevice(page, { finePointer: false, saveData: false });
  await page.goto(`${server.origin}/?hooks`);
  await expect(page.getByTestId("landing-page")).toBeVisible();
  server.missing = /\/EditorPage-/;

  await page.getByTestId("open-demo").click();
  const problem = page.getByTestId("load-problem");
  await expect(problem).toContainText("A newer version of Reticula is available");
  await expect(page).toHaveURL(`${server.origin}/app?hooks=`);
  // The document the tab was opened with, and one automatic attempt.
  expect(server.documents()).toBe(2);

  server.missing = null;
  await problem.getByRole("button", { name: "Reload" }).click();
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await expect(problem).toHaveCount(0);
});

test("loading the new build once is enough when it is complete", async ({ page }) => {
  server.missing = /\/EditorPage-/;
  server.onDocument = () => {
    if (server.documents() > 1) {
      server.missing = null;
    }
  };
  await page.goto(`${server.origin}/app?hooks`, { waitUntil: "commit" });
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await expect(page.getByTestId("load-problem")).toHaveCount(0);
  expect(server.documents()).toBe(2);
});

test("a part of the editor that is gone leaves a message in place of a blank page", async ({
  page,
}) => {
  await page.goto(`${server.origin}/app?hooks`);
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  server.missing = /\/dialogs-/;

  const reloaded = page.waitForEvent("load");
  await page.getByTestId("workbooks-btn").click();
  await reloaded;
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  expect(server.documents()).toBe(2);

  await page.getByTestId("workbooks-btn").click();
  await expect(page.getByTestId("load-problem")).toContainText("A newer version");
  expect(server.documents()).toBe(2);
});

test("a key command whose code is gone is not ignored", async ({ page }) => {
  server.missing = /\/later-/;
  const openCommands = async () => {
    await expect(page.getByTestId("engine-status")).toHaveText("ready");
    await firstScreen(page);
    await page.getByTestId("grid").focus();
    await page.keyboard.press("ControlOrMeta+/");
  };
  await page.goto(`${server.origin}/app?hooks`);
  const reloaded = page.waitForEvent("load");
  await openCommands();
  await reloaded;
  expect(server.documents()).toBe(2);

  await openCommands();
  const problem = page.getByTestId("load-problem");
  await expect(problem).toContainText("A newer version");
  // What works without it stays.
  await expect(page.getByTestId("grid")).toBeVisible();
  expect(server.documents()).toBe(2);

  server.missing = null;
  await problem.getByRole("button", { name: "Reload" }).click();
  await expect(problem).toHaveCount(0);
  await openCommands();
  await expect(page.getByTestId("palette-input")).toBeFocused();
});

test("a workbook that is open elsewhere is not shown without the lock over it", async ({
  page,
  context,
}) => {
  // From the start, so that the browser does not have it either.
  server.missing = /\/later-/;
  await page.goto(`${server.origin}/app?hooks`);
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  const other = await context.newPage();
  await other.goto(`${server.origin}/app?hooks`, { waitUntil: "commit" });
  const problem = other.getByTestId("load-problem");
  await expect(problem).toContainText("A newer version", { timeout: 20_000 });
  await expect(other.getByTestId("grid")).toHaveCount(0);
  expect(server.documents()).toBe(3);

  server.missing = null;
  await problem.getByRole("button", { name: "Reload" }).click();
  await expect(other.locator(".lock-overlay")).toBeVisible();
});

test("a message in place of a page stays when the address changes", async ({ page }) => {
  await page.goto(`${server.origin}/app?hooks`);
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  server.missing = /\/dialogs-/;
  const reloaded = page.waitForEvent("load");
  await page.getByTestId("workbooks-btn").click();
  await reloaded;
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await page.getByTestId("workbooks-btn").click();
  const problem = page.getByTestId("load-problem");
  await expect(problem).toContainText("A newer version");

  // Loads the page that is already there, which is not the one that failed to appear.
  await page.evaluate(() => {
    window.location.hash = "part";
  });
  await page.waitForTimeout(300);
  await expect(problem).toContainText("A newer version");
});

test("an engine that is gone after a deploy is loaded afresh, and only then given up", async ({
  page,
}) => {
  await emulateDevice(page, { finePointer: true, saveData: false });
  const build = await readBuild();
  await page.goto(`${server.origin}/?hooks`);
  await expect
    .poll(() => build.editor.map((name) => server.hits(name)))
    .toEqual(build.editor.map(() => 1));
  // The editor's code is already here; what it starts belongs to a build that is not there any more.
  server.missing = /\/worker-/;

  await page.getByTestId("open-demo").click();
  const problem = page.getByTestId("load-problem");
  await expect(problem).toContainText("A newer version of Reticula is available");
  expect(server.documents()).toBe(2);

  server.missing = null;
  await problem.getByRole("button", { name: "Reload" }).click();
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await expect(problem).toHaveCount(0);
});

test("leaving a page while a part of it is loading is not a failure", async ({ page }) => {
  await emulateDevice(page, { finePointer: false, saveData: false });
  await page.goto(`${server.origin}/`);
  await expect(page.getByTestId("landing-page")).toBeVisible();
  const asked = page.waitForRequest(/\/assets\/WorkbooksPage-/);
  server.slow = /\/WorkbooksPage-/;
  await page.getByRole("link", { name: "Workbooks" }).click();
  await asked;
  // Some browsers report the abort when the navigation starts, some when it ends.
  server.slow = /\/WorkbooksPage-|^\/privacy$/;
  const left = page.waitForURL(`${server.origin}/privacy`);
  await page.evaluate(() => {
    window.location.assign("/privacy");
  });
  await left;
  await expect(page.getByTestId("privacy-page")).toBeVisible();
  expect(server.documents()).toBe(2);
});

test("without a network the page in view stays, with a message", async ({ page, context }) => {
  await emulateDevice(page, { finePointer: false, saveData: false });
  await page.goto(`${server.origin}/`);
  await expect(page.getByTestId("landing-page")).toBeVisible();
  await context.setOffline(true);
  await page.getByTestId("nav-benchmarks").click();
  await expect(page.getByTestId("load-problem")).toContainText("You appear to be offline");
  await expect(page.getByTestId("landing-page")).toBeVisible();
  await expect(page).toHaveURL(`${server.origin}/`);
  expect(server.documents()).toBe(1);
});
