// Takes the pictures of the landing page from the product itself.
//
//   node tests/media/capture.mjs [--dist <directory>] [--out <directory>] [--only <shot>,...]
//
// Needs a production build (`cargo xtask build`), the debug server (`cargo build
// --package reticula-server`) and a database to run it against (`cargo xtask db up`,
// or RETICULA_TEST_DATABASE_URL). The same build of Chromium makes the same files
// again, as long as what is in the pictures has not changed: the sample and the
// recorded demos come from seeds, and what depends on the clock is left out.

import { spawn } from "node:child_process";
import { mkdir, mkdtemp, readdir, rm, stat, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { chromium, expect } from "@playwright/test";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const DATABASE_URL =
  process.env.RETICULA_TEST_DATABASE_URL ??
  "postgres://reticula:reticula_local_dev@127.0.0.1:5434/reticula";
/** What all pictures together may take. */
const MAX_BYTES = 600_000;

const { values: options } = parseArgs({
  options: {
    dist: { type: "string", default: join(ROOT, "apps/web/dist") },
    out: { type: "string", default: join(ROOT, "apps/web/public/media") },
    only: { type: "string" },
  },
});

/** What is entered into the pictures' sheets, in the language around them. */
const WORDS = {
  en: {
    budget: "Trip budget",
    rows: ["Flights", "Hotel", "Rail pass", "Meals", "Museums"],
    amount: "Amount",
    total: "Total",
    version: "Before the rail pass",
  },
  zh: {
    budget: "旅行预算",
    rows: ["机票", "酒店", "铁路通票", "餐饮", "博物馆"],
    amount: "金额",
    total: "合计",
    version: "购买通票之前",
  },
};

async function freePort() {
  const server = createServer();
  await new Promise((listening) => server.listen(0, "127.0.0.1", listening));
  const address = server.address();
  await new Promise((closed) => server.close(closed));
  if (address === null || typeof address === "string") {
    throw new Error("no port on the loopback interface");
  }
  return address.port;
}

/** The collaboration server with its development sign-in, serving the build. */
async function startServer() {
  const port = await freePort();
  const origin = `http://127.0.0.1:${port}`;
  const data = await mkdtemp(join(tmpdir(), "reticula-media-"));
  const suffix = process.platform === "win32" ? ".exe" : "";
  const log = [];
  const child = spawn(join(ROOT, `target/debug/reticula-server${suffix}`), [], {
    env: {
      ...process.env,
      BIND_ADDR: `127.0.0.1:${port}`,
      RETICULA_STATIC_DIR: resolve(options.dist),
      RETICULA_DATA_DIR: data,
      RETICULA_DATABASE_URL: DATABASE_URL,
      RETICULA_PUBLIC_ORIGIN: origin,
      RETICULA_AUTH_MODE: "dev-mock",
      RETICULA_LLM_MODE: "mock",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stdout.on("data", (chunk) => log.push(chunk));
  child.stderr.on("data", (chunk) => log.push(chunk));
  const stop = async () => {
    if (child.exitCode === null) {
      const exited = new Promise((resolve) => child.once("exit", resolve));
      child.kill();
      await exited;
    }
    await rm(data, { recursive: true, force: true });
  };
  for (let attempt = 0; attempt < 150; attempt++) {
    const ready = await fetch(`${origin}/readyz`).then(
      (response) => response.ok,
      () => false,
    );
    if (ready) {
      return { origin, stop };
    }
    if (child.exitCode !== null) {
      break;
    }
    await new Promise((later) => setTimeout(later, 100));
  }
  await stop();
  throw new Error(`the server did not start:\n${log.join("")}`);
}

/** Waits until everything in view has been drawn. */
async function settled(page) {
  await page.waitForFunction(
    () => {
      const stats = window.__reticula?.stats();
      return stats !== undefined && stats.cellsDrawn > 0 && stats.missingCells === 0;
    },
    undefined,
    { timeout: 60_000 },
  );
}

async function openEditor(page, origin, search = "") {
  await page.goto(`${origin}/app?hooks&renderer=canvas2d${search}`);
  await expect(page.getByTestId("engine-status")).toHaveText(/./);
  await settled(page);
  const tip = page.locator(".demo-tip .tip-close");
  if (await tip.count()) {
    await tip.click();
  }
}

async function enter(page, row, col, text) {
  await page.evaluate(([r, c]) => window.__reticula?.select(r, c), [row, col]);
  await page.evaluate(() => window.__reticula?.stateDigest());
  await expect(page.locator(".formulabar[data-inspected]")).toBeVisible();
  const input = page.getByTestId("formula-input");
  await input.click();
  await expect(page.locator(".formula-cm-wrap.focused")).toBeVisible();
  await input.fill(text);
  await input.press("Enter");
  await page.evaluate(() => window.__reticula?.stateDigest());
  await settled(page);
}

/** A part of the page, from the top of one element to the bottom of another. */
async function between(page, first, last) {
  const [top, bottom] = [
    await page.locator(first).first().boundingBox(),
    await page.locator(last).first().boundingBox(),
  ];
  if (!top || !bottom) {
    throw new Error(`${first} or ${last} is not on the page`);
  }
  const x = Math.floor(Math.min(top.x, bottom.x));
  const right = Math.ceil(Math.max(top.x + top.width, bottom.x + bottom.width));
  const y = Math.round(top.y);
  return { x, y, width: right - x, height: Math.round(bottom.y + bottom.height) - y };
}

/** The million-row sample with a formula being edited, which has no words of the interface in it. */
async function sheet({ page, origin }) {
  await openEditor(page, origin);
  await page.evaluate(() => {
    window.__reticula?.scrollTo({ left: 1_350, top: 0 });
    window.__reticula?.select(5, 20);
  });
  await settled(page);
  await expect(page.locator(".formulabar[data-inspected]")).toBeVisible();
  const input = page.getByTestId("formula-input");
  await input.click();
  await input.fill('=SUMIFS(M2:M100001,S2:S100001,">=4",P2:P100001,0)');
  await page.waitForTimeout(500);
  return between(page, ".formulabar", ".sheet");
}

/** The end of the sample, as far zoomed out as it goes. */
async function zoom({ page, origin }) {
  await openEditor(page, origin);
  const out = page.getByTestId("zoom-out");
  for (let steps = 0; steps < 20 && (await out.getAttribute("aria-disabled")) !== "true"; steps++) {
    await out.click();
  }
  await page.evaluate(() => {
    const row = 6;
    window.__reticula?.select(999_982, 12);
    // Not quite to the end, where the last row's number would be cut off.
    window.__reticula?.scrollTo({ left: 0, top: (1_000_000 - 74) * row });
  });
  await settled(page);
  await page.waitForTimeout(300);
  return between(page, ".sheet", ".sheet");
}

/** Two replicas in the simulator, with one link held back. */
async function simulator({ page, origin, words }) {
  await page.goto(`${origin}/sim?renderer=canvas2d`);
  await expect(page.getByTestId("sim-replica-card-1")).toBeVisible({ timeout: 30_000 });
  const put = async (replica, row, col, text) => {
    await page.getByTestId(`sim-row-${replica}`).fill(String(row));
    await page.getByTestId(`sim-col-${replica}`).fill(String(col));
    await page.getByTestId(`sim-value-${replica}`).fill(text);
    await page.getByTestId(`sim-apply-${replica}`).click();
  };
  const amounts = ["420", "610", "180", "240", "55"];
  for (const [row, name] of words.rows.entries()) {
    await put(0, row, 0, name);
    await put(0, row, 1, amounts[row]);
  }
  await put(0, 5, 0, words.total);
  await put(0, 5, 1, "=SUM(B1:B5)");
  const all = page.getByTestId("sim-deliver-all");
  if ((await all.getAttribute("aria-disabled")) !== "true") {
    await all.click();
  }
  await page.getByTestId("sim-link-1").selectOption("delayed");
  await put(1, 2, 1, "95");
  await page.getByTestId("sim-insert-row-0").click();
  await put(0, 0, 0, words.budget);
  await page.mouse.move(0, 0);
  await page.waitForTimeout(300);
  return between(page, ".sim-server-banner", ".sim-replicas-grid");
}

/** A recorded run of the AI Analyst, with the review of its draft. */
async function review({ page, origin }) {
  await openEditor(page, origin);
  await page.getByTestId("ai-panel-btn").click();
  await page.getByTestId("ai-demo-conflict-and-closure-demo").click();
  await expect(page.getByTestId("ai-draft-banner")).toBeVisible();
  await page.getByTestId("ai-replay-review-btn").click();
  await expect(page.getByTestId("ai-diff-review")).toBeVisible();
  await page.getByTestId("conflict-take-draft-B3").click();
  await page.mouse.move(0, 0);
  await page.waitForTimeout(300);
  return between(page, ".formatbar", ".sheet");
}

/** A shared workbook, seen as it was some revisions ago. */
async function history({ page, origin, words }) {
  // When something happened is not the same in two runs.
  const fixed = Date.UTC(2026, 10, 24, 9, 0, 0);
  await page.route(/\/api\/workbooks\/[^/]+\/(?:history|versions)/, async (route) => {
    const response = await route.fetch();
    let entry = 0;
    const body = (await response.text()).replace(/"createdAt":"[^"]+"/g, () => {
      entry++;
      return `"createdAt":"${new Date(fixed - entry * 47_000).toISOString()}"`;
    });
    await route.fulfill({ response, body });
  });
  await openEditor(page, origin);
  await page.getByTestId("cloud-menu-btn").click();
  await page.getByTestId("dev-login-alice").click();
  await expect(page.getByTestId("collab-user-badge")).toBeVisible();
  await page.getByTestId("new-cloud-title-input").fill(words.budget);
  await page.getByTestId("new-cloud-sheet-btn").click();
  await expect(page.getByTestId("collab-status-pill")).toBeVisible();
  await settled(page);
  const amounts = ["420", "610", "180", "240", "55"];
  for (const [row, name] of words.rows.entries()) {
    await enter(page, row, 0, name);
    await enter(page, row, 1, amounts[row]);
  }
  await enter(page, 5, 0, words.total);
  await enter(page, 5, 1, "=SUM(B1:B5)");
  const sent = () => page.evaluate(() => window.__reticula?.collabStatus().pending);
  await expect.poll(sent).toBe(0);
  await page.getByTestId("history-btn").click();
  await page.getByTestId("history-version-name-input").fill(words.version);
  await page.getByTestId("history-save-version-btn").click();
  const version = page.locator('[data-testid="history-versions-list"] button').first();
  await expect(version).toBeVisible();
  await page.getByTestId("history-close-btn").click();
  await enter(page, 2, 1, "95");
  await enter(page, 0, 1, "395");
  await expect.poll(sent).toBe(0);
  await page.getByTestId("history-btn").click();
  await expect(page.getByTestId("history-entries-list")).toBeVisible();
  await version.click();
  await expect(version).toHaveAttribute("aria-current", "true");
  await settled(page);
  await page.waitForTimeout(500);
  // How long it took to get there, and how much is stored by now.
  await page.locator(".history-drawer-subtitle").evaluate((line) => {
    line.textContent = (line.textContent ?? "").replace(/\s*[(（][^)）]*[)）]$/, "");
  });
  await page.addStyleTag({ content: ".storage-badge { display: none }" });
  await page.mouse.move(0, 0);
  return between(page, ".titlebar", ".sheet");
}

/** What other sites show of this one: the heading of the page above the sample. */
async function preview({ page, origin }) {
  const clip = await sheet({ page, origin });
  const png = await page.screenshot({ clip, caret: "hide" });
  await page.goto(`${origin}/`);
  const heading = (await page.locator("h1").textContent()) ?? "";
  await page.evaluate(
    ({ heading, picture }) => {
      const [brand, title, image] = ["p", "h1", "img"].map((name) => document.createElement(name));
      brand.textContent = "Reticula";
      title.textContent = heading;
      image.src = picture;
      brand.style.cssText = "margin: 0; font: 600 26px/1 'Segoe UI', system-ui, sans-serif";
      title.style.cssText =
        "margin: 22px 0 40px; font: 650 58px/1.12 'Segoe UI', system-ui, sans-serif; letter-spacing: -0.02em";
      image.style.cssText = "width: 1200px; border: 1px solid #d1d5da; border-radius: 6px";
      document.body.replaceChildren(brand, title, image);
      document.body.style.cssText =
        "box-sizing: border-box; height: 630px; padding: 52px 64px 0; overflow: hidden; background: #fff; color: #1f2328";
      return image.decode();
    },
    { heading, picture: `data:image/png;base64,${png.toString("base64")}` },
  );
  return { x: 0, y: 0, width: 1200, height: 630 };
}

const BOTH = ["light", "dark"];

/** The pictures. Those with words of the interface in them are taken in both languages. */
const SHOTS = {
  sheet: { take: sheet, viewport: { width: 1200, height: 640 }, scale: 2, themes: BOTH },
  zoom: { take: zoom, viewport: { width: 738, height: 660 }, scale: 1, themes: BOTH },
  sim: {
    take: simulator,
    viewport: { width: 1040, height: 760 },
    scale: 1,
    themes: BOTH,
    words: true,
  },
  review: {
    take: review,
    viewport: { width: 1000, height: 640 },
    scale: 1,
    themes: BOTH,
    words: true,
  },
  history: {
    take: history,
    viewport: { width: 1000, height: 640 },
    scale: 1,
    themes: BOTH,
    words: true,
  },
  og: { take: preview, viewport: { width: 1200, height: 640 }, scale: 1, themes: ["light"] },
};
const LANGUAGES = { en: "en-US", zh: "zh-CN" };

/** A picture as WebP, whichever way of encoding it takes less. */
async function encode(page, png) {
  const bytes = await page.evaluate(
    async ({ data, qualities }) => {
      const picture = await createImageBitmap(await (await fetch(data)).blob());
      const canvas = new OffscreenCanvas(picture.width, picture.height);
      canvas.getContext("2d")?.drawImage(picture, 0, 0);
      const blobs = await Promise.all(
        qualities.map((quality) => canvas.convertToBlob({ type: "image/webp", quality })),
      );
      const least = blobs.reduce((one, other) => (other.size < one.size ? other : one));
      const buffer = new Uint8Array(await least.arrayBuffer());
      let text = "";
      for (const byte of buffer) {
        text += String.fromCharCode(byte);
      }
      return btoa(text);
    },
    // A quality of 1 is lossless, which suits flat areas better.
    { data: `data:image/png;base64,${png.toString("base64")}`, qualities: [0.6, 1] },
  );
  return Buffer.from(bytes, "base64");
}

async function main() {
  const only = options.only?.split(",");
  const server = await startServer();
  const browser = await chromium.launch();
  await mkdir(options.out, { recursive: true });
  try {
    const blank = await browser.newPage();
    for (const [name, { take, viewport, scale, themes, words }] of Object.entries(SHOTS)) {
      if (only && !only.includes(name)) {
        continue;
      }
      for (const language of words ? Object.keys(LANGUAGES) : ["en"]) {
        for (const colorScheme of themes) {
          const context = await browser.newContext({
            viewport,
            deviceScaleFactor: scale,
            colorScheme,
            locale: LANGUAGES[language],
            timezoneId: "UTC",
            reducedMotion: "reduce",
          });
          const page = await context.newPage();
          const clip = await take({ page, origin: server.origin, words: WORDS[language] });
          const png = await page.screenshot({ clip, caret: "hide", animations: "disabled" });
          await context.close();
          const webp = await encode(blank, png);
          const parts = [
            name,
            ...(words ? [language] : []),
            ...(themes.length > 1 ? [colorScheme] : []),
          ];
          const file = `${parts.join("-")}.webp`;
          await writeFile(join(options.out, file), webp);
          console.log(`${file}: ${clip.width} × ${clip.height}, ${webp.length} B`);
        }
      }
    }
  } finally {
    await browser.close();
    await server.stop();
  }
  let total = 0;
  for (const file of await readdir(options.out)) {
    total += (await stat(join(options.out, file))).size;
  }
  console.log(`all of ${options.out}: ${total} B of ${MAX_BYTES} B`);
  if (total > MAX_BYTES) {
    process.exitCode = 1;
  }
}

await main();
