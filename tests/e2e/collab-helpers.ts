// Steps the collaborative end-to-end tests share: a server to talk to, and
// signing in, creating, sharing and editing a workbook through the page.
import { type ChildProcess, spawn } from "node:child_process";
import { mkdtemp, open, rm, stat } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { expect, type Page } from "@playwright/test";

export type DevAccount = "alice" | "bob" | "carol";

function accountDisplayName(account: DevAccount): string {
  switch (account) {
    case "alice":
      return "Alice";
    case "bob":
      return "Bob";
    case "carol":
      return "Carol";
  }
}

export async function withFileLock<T>(lockName: string, fn: () => Promise<T>): Promise<T> {
  const lockPath = join(tmpdir(), lockName);
  const deadline = Date.now() + 120_000;
  for (;;) {
    try {
      const handle = await open(lockPath, "wx");
      await handle.close();
      break;
    } catch (err) {
      const code = (err as NodeJS.ErrnoException).code;
      if (code !== "EEXIST") {
        throw err;
      }
      try {
        const info = await stat(lockPath);
        if (Date.now() - info.mtimeMs > 120_000) {
          await rm(lockPath, { force: true });
          continue;
        }
      } catch {
        continue;
      }
      if (Date.now() > deadline) {
        throw new Error(`Timed out waiting for file lock ${lockPath}`);
      }
      await new Promise((r) => setTimeout(r, 100));
    }
  }
  try {
    return await fn();
  } finally {
    await rm(lockPath, { force: true });
  }
}

const DEFAULT_TEST_DB_URL = "postgres://reticula:reticula_local_dev@127.0.0.1:5434/reticula";

export interface CollabServerHandle {
  readonly baseUrl: string;
  stop(): Promise<void>;
}

async function allocatePort(): Promise<number> {
  return new Promise((resolvePort, reject) => {
    const server = createServer();
    server.unref();
    server.on("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const addr = server.address();
      if (!addr || typeof addr === "string") {
        server.close(() => reject(new Error("Could not allocate loopback port")));
        return;
      }
      const { port } = addr;
      server.close((err) => {
        if (err) {
          reject(err);
        } else {
          resolvePort(port);
        }
      });
    });
  });
}

export async function startCollabServer(): Promise<CollabServerHandle> {
  const databaseUrl = process.env.RETICULA_TEST_DATABASE_URL ?? DEFAULT_TEST_DB_URL;
  const port = await allocatePort();
  const baseUrl = `http://127.0.0.1:${port}`;
  const dataDir = await mkdtemp(join(tmpdir(), "reticula-e2e-data-"));
  const binName = process.platform === "win32" ? "reticula-server.exe" : "reticula-server";
  const binPath = resolve(process.cwd(), "target/debug", binName);
  const staticDir = resolve(process.cwd(), "apps/web/dist");

  const logs: string[] = [];
  const child: ChildProcess = spawn(binPath, [], {
    env: {
      ...process.env,
      BIND_ADDR: `127.0.0.1:${port}`,
      RETICULA_STATIC_DIR: staticDir,
      RETICULA_DATA_DIR: dataDir,
      RETICULA_DATABASE_URL: databaseUrl,
      RETICULA_PUBLIC_ORIGIN: baseUrl,
      RETICULA_AUTH_MODE: "dev-mock",
      RETICULA_LLM_MODE: "mock",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });

  child.stdout?.on("data", (chunk: Buffer) => logs.push(chunk.toString()));
  child.stderr?.on("data", (chunk: Buffer) => logs.push(chunk.toString()));

  let exitError: Error | null = null;
  child.once("exit", (code, signal) => {
    exitError = new Error(
      `reticula-server exited unexpectedly (code=${code}, signal=${signal}):\n${logs.join("")}`,
    );
  });

  const deadline = Date.now() + 15_000;
  for (;;) {
    if (exitError) {
      await rm(dataDir, { recursive: true, force: true });
      throw exitError;
    }
    try {
      const resp = await fetch(`${baseUrl}/readyz`);
      if (resp.ok) {
        break;
      }
    } catch {
      // Server still starting up.
    }
    if (Date.now() > deadline) {
      child.kill();
      await rm(dataDir, { recursive: true, force: true });
      throw new Error(`Timed out waiting for reticula-server /readyz:\n${logs.join("")}`);
    }
    await new Promise((r) => setTimeout(r, 100));
  }

  return {
    baseUrl,
    async stop() {
      child.kill();
      await new Promise<void>((resolveExit) => {
        if (child.exitCode !== null || child.signalCode !== null) {
          resolveExit();
          return;
        }
        const timer = setTimeout(resolveExit, 3_000);
        child.once("exit", () => {
          clearTimeout(timer);
          resolveExit();
        });
      });
      await rm(dataDir, { recursive: true, force: true });
    },
  };
}

export async function settled(page: Page): Promise<void> {
  await page.waitForFunction(() => {
    const stats = window.__reticula?.stats();
    return stats !== undefined && stats.pagesHeld > 0 && stats.missingCells === 0;
  });
}

export async function signInDev(page: Page, baseUrl: string, account: DevAccount): Promise<void> {
  await page.goto(`${baseUrl}/app?hooks`);
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await page.getByTestId("cloud-menu-btn").click();
  await expect(page.getByTestId("collab-modal")).toBeVisible();
  await page.getByTestId(`dev-login-${account}`).click();
  await expect(page.getByTestId("collab-user-badge")).toContainText(accountDisplayName(account));
}

export async function createAndShareSheet(
  page: Page,
  title: string,
  singleUse = false,
): Promise<{ readonly collabUrl: string; readonly joinPath: string; readonly docId: string }> {
  await page.getByTestId("new-cloud-title-input").fill(title);
  await page.getByTestId("new-cloud-sheet-btn").click();
  await expect(page.getByTestId("collab-status-pill")).toHaveText("Synced", { timeout: 10_000 });
  await settled(page);

  const collabUrl = page.url();
  const parsed = new URL(collabUrl);
  const docId = parsed.searchParams.get("collab");
  if (!docId) {
    throw new Error(`Expected ?collab= in URL, got ${collabUrl}`);
  }

  await page.getByTestId("cloud-menu-btn").click();
  if (singleUse) {
    await page.getByTestId("share-single-use-toggle").check();
  }
  await page.getByTestId("create-share-link-btn").click();
  const shareLinkInput = page.getByTestId("share-link-url");
  await expect(shareLinkInput).toHaveValue(/\/join\//, { timeout: 10_000 });
  const fullShareUrl = await shareLinkInput.inputValue();
  const joinPath = new URL(fullShareUrl).pathname;
  await page.getByTestId("collab-modal-close").click();
  await expect(page.getByTestId("collab-modal")).toBeHidden();
  return { collabUrl, joinPath, docId };
}

export async function joinViaSharePath(
  page: Page,
  baseUrl: string,
  joinPath: string,
  account: DevAccount,
): Promise<void> {
  await page.goto(`${baseUrl}${joinPath}?hooks`);
  await expect(page.getByTestId("collab-auth-panel")).toBeVisible({ timeout: 10_000 });
  await page.getByTestId(`dev-login-${account}`).click();
  await expect(page.getByTestId("collab-status-pill")).toHaveText("Synced", { timeout: 10_000 });
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await settled(page);
}

async function flushWorkerAndReact(page: Page): Promise<void> {
  await page.evaluate(async () => {
    await window.__reticula?.stateDigest();
    await new Promise<void>((resolve) => {
      setTimeout(resolve, 0);
    });
  });
}

export async function editCell(
  page: Page,
  row: number,
  col: number,
  address: string,
  raw: string,
): Promise<void> {
  await settled(page);
  await page.evaluate(
    ([r, c]) => {
      window.__reticula?.select(r, c);
    },
    [row, col] as const,
  );
  await flushWorkerAndReact(page);
  await expect(page.getByTestId("name-box")).toHaveText(address);
  const formulaInput = page.getByTestId("formula-input");
  await expect(formulaInput).toHaveAttribute("contenteditable", "true");
  // Settle CodeMirror's delayed focus-change update before replacing text so its
  // selection sync cannot collapse Playwright's `fill()` selection to offset 0.
  await formulaInput.click();
  await expect(page.locator(".formula-cm-wrap.focused")).toBeVisible();
  await flushWorkerAndReact(page);
  await formulaInput.fill(raw);
  await expect(formulaInput).toHaveText(raw);
  await formulaInput.press("Enter");
  await flushWorkerAndReact(page);
  await settled(page);
}
