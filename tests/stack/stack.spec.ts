import { execFileSync } from "node:child_process";
import { createHash, randomBytes, randomUUID } from "node:crypto";
import { expect, type Page, test } from "@playwright/test";
import { createAndShareSheet, editCell, settled } from "../e2e/collab-helpers.ts";

// The stack `cargo xtask stack` started. The release image only signs people
// in through GitHub, so the test writes two sessions straight into the
// database, the way a completed sign-in leaves them; the GitHub round trip
// itself is not covered here.
const baseUrl = process.env.RETICULA_STACK_URL ?? "http://127.0.0.1:3200";
const envFile = process.env.RETICULA_STACK_ENV_FILE;
const project = process.env.RETICULA_STACK_PROJECT ?? "reticula-stack";

function compose(...args: string[]): string {
  if (!envFile) {
    throw new Error("RETICULA_STACK_ENV_FILE is not set: run this through `cargo xtask stack`");
  }
  return execFileSync(
    "docker",
    [
      "compose",
      "--project-name",
      project,
      "--file",
      "infra/compose.yml",
      "--env-file",
      envFile,
      ...args,
    ],
    { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
  );
}

function psql(sql: string): string {
  return compose(
    "exec",
    "-T",
    "postgres",
    "psql",
    "--username=reticula",
    "--dbname=reticula",
    "--set=ON_ERROR_STOP=1",
    "--tuples-only",
    "--no-align",
    "--command",
    sql,
  ).trim();
}

/** Creates a user with a signed-in session and returns the session cookie's value. */
function seedSession(name: string): string {
  const secret = randomBytes(32);
  const hash = createHash("sha256").update(secret).digest("hex");
  const id = randomUUID();
  psql(
    `INSERT INTO users (id, name, avatar_url) VALUES ('${id}', '${name}', '');
     INSERT INTO sessions (token_hash, user_id, expires_at)
       VALUES ('\\x${hash}', '${id}', now() + interval '1 day');`,
  );
  return secret.toString("base64url");
}

async function waitReady(): Promise<void> {
  await expect
    .poll(
      async () => {
        try {
          return (await fetch(`${baseUrl}/readyz`)).status;
        } catch {
          return 0;
        }
      },
      { timeout: 60_000, intervals: [250] },
    )
    .toBe(200);
}

async function synced(page: Page, timeout = 30_000): Promise<void> {
  await expect(page.getByTestId("collab-status-pill")).toHaveText("Synced", { timeout });
}

interface MemoryUse {
  readonly name: string;
  readonly usage: string;
}

function memoryUse(): MemoryUse[] {
  const ids = compose("ps", "--quiet").trim().split(/\s+/);
  return execFileSync(
    "docker",
    ["stats", "--no-stream", "--format", "{{.Name}}\t{{.MemUsage}}", ...ids],
    { encoding: "utf8" },
  )
    .trim()
    .split("\n")
    .map((line) => {
      const [name = "", usage = ""] = line.split("\t");
      return { name, usage };
    });
}

test("compose stack: sync, server SIGKILL, database restart and recreated containers", async ({
  browser,
}) => {
  await waitReady();

  // What the release image exposes without a session.
  expect((await fetch(`${baseUrl}/healthz`)).status).toBe(200);
  expect((await fetch(`${baseUrl}/api/auth/me`)).status).toBe(401);
  const devLogin = await fetch(`${baseUrl}/api/auth/dev-login`, {
    method: "POST",
    headers: { origin: baseUrl, "content-type": "application/json" },
    body: JSON.stringify({ account: "alice" }),
  });
  expect(devLogin.status, "the release image must not offer dev-mock sign-in").toBe(404);

  const aliceToken = seedSession("Alice");
  const bobToken = seedSession("Bob");
  const signedIn = async (token: string) => {
    const context = await browser.newContext();
    await context.addCookies([{ name: "reticula_session", value: token, url: baseUrl }]);
    return context;
  };

  const aliceCtx = await signedIn(aliceToken);
  const bobCtx = await signedIn(bobToken);
  try {
    const alice = await aliceCtx.newPage();
    const bob = await bobCtx.newPage();

    await alice.goto(`${baseUrl}/app?hooks`);
    await expect(alice.getByTestId("engine-status")).toHaveText("ready");
    await alice.getByTestId("cloud-menu-btn").click();
    await expect(alice.getByTestId("collab-user-badge")).toContainText("Alice");
    const { collabUrl, joinPath } = await createAndShareSheet(alice, "Stack run-through");

    await bob.goto(`${baseUrl}${joinPath}?hooks`);
    await synced(bob);
    await settled(bob);

    await editCell(alice, 1, 1, "B2", "10");
    await editCell(alice, 2, 1, "B3", "20");
    await editCell(alice, 3, 1, "B4", "=SUM(B2:B3)");
    await synced(alice);
    await expect.poll(() => bob.evaluate(() => window.__reticula?.textAt(3, 1))).toBe("30");

    // 1. Kill the server outright. Edits made while it is down wait in the
    //    browser and go through once it is back.
    compose("kill", "--signal", "SIGKILL", "server");
    await expect(alice.getByTestId("collab-status-pill")).not.toHaveText("Synced", {
      timeout: 15_000,
    });
    await editCell(alice, 0, 0, "A1", "during-outage");
    compose("start", "server");
    await waitReady();
    await synced(alice);
    await synced(bob);
    await expect
      .poll(() => bob.evaluate(() => window.__reticula?.textAt(0, 0)), { timeout: 10_000 })
      .toBe("during-outage");

    // 2. Restart the database under a running server.
    compose("restart", "postgres");
    await waitReady();
    await editCell(bob, 0, 2, "C1", "after-db-restart");
    await synced(bob);
    await expect
      .poll(() => alice.evaluate(() => window.__reticula?.textAt(0, 2)), { timeout: 10_000 })
      .toBe("after-db-restart");

    // 2.5 Start a `[slow]` AI analyst run on the collaborative workbook, SIGKILL
    //     the server container mid-run, restart it, and verify durable step recovery.
    const docId = new URL(collabUrl).searchParams.get("collab");
    expect(docId).toBeTruthy();
    const startRunRes = await alice.request.post(`${baseUrl}/api/workbooks/${docId}/agent/runs`, {
      headers: { origin: baseUrl },
      data: { prompt: "[slow] Stack restart recovery run" },
    });
    expect(startRunRes.status()).toBe(201);
    const startedRun = (await startRunRes.json()) as { id: string };
    await expect
      .poll(
        async () => {
          const r = await alice.request.get(
            `${baseUrl}/api/workbooks/${docId}/agent/runs/${startedRun.id}`,
          );
          const body = (await r.json()) as { run: { status: string }; steps: unknown[] };
          return body.run.status === "running" && body.steps.length >= 2;
        },
        { timeout: 15_000, intervals: [150] },
      )
      .toBe(true);
    compose("kill", "--signal", "SIGKILL", "server");
    compose("start", "server");
    await waitReady();
    await expect
      .poll(
        async () => {
          const r = await alice.request.get(
            `${baseUrl}/api/workbooks/${docId}/agent/runs/${startedRun.id}`,
          );
          const body = (await r.json()) as { run: { status: string; resumeCount: number } };
          return `${body.run.status}:${body.run.resumeCount}`;
        },
        { timeout: 25_000, intervals: [200] },
      )
      .toBe("waiting_review:1");

    const memory = memoryUse();
    for (const entry of memory) {
      const line = `${entry.name}: ${entry.usage}`;
      test.info().annotations.push({ type: "memory", description: line });
      console.log(`memory ${line}`);
    }

    // 3. Replace both containers; only the volumes carry the workbook over.
    //    A browser with no local copy must get everything from the server.
    compose("up", "--detach", "--wait", "--force-recreate");
    await waitReady();
    const freshCtx = await signedIn(aliceToken);
    try {
      const fresh = await freshCtx.newPage();
      await fresh.goto(collabUrl);
      await synced(fresh);
      await settled(fresh);
      await expect.poll(() => fresh.evaluate(() => window.__reticula?.textAt(3, 1))).toBe("30");
      expect(await fresh.evaluate(() => window.__reticula?.textAt(0, 0))).toBe("during-outage");
      expect(await fresh.evaluate(() => window.__reticula?.textAt(0, 2))).toBe("after-db-restart");
      const freshDigest = await fresh.evaluate(() => window.__reticula?.stateDigest());
      await synced(alice);
      expect(freshDigest).toBe(await alice.evaluate(() => window.__reticula?.stateDigest()));
    } finally {
      await freshCtx.close();
    }
  } finally {
    await aliceCtx.close();
    await bobCtx.close();
  }
});
