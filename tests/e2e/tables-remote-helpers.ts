// A server behind a proxy that can lose, damage and refuse what passes through it,
// and the client of tables on that server without anything on the screen.
import { type ChildProcess, spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { mkdtemp, rm } from "node:fs/promises";
import { createServer, type IncomingMessage, request, type ServerResponse } from "node:http";
import { connect as connectTo, createServer as createTcpServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { expect, type Page } from "@playwright/test";
import type { TableRecord } from "../../packages/protocol/src/generated/collab-api.ts";
import type { DevAccount } from "./collab-helpers.ts";
import { listen } from "./listen.ts";

type Tables = Awaited<ReturnType<NonNullable<Window["__reticula"]>["tables"]>>;
export type Client = InstanceType<Tables["TableClient"]>;

/** What passes through the proxy. */
export interface Passing {
  readonly method: string;
  readonly path: string;
  /** The bytes of a table that are asked for: from the first to before the second. */
  readonly range: readonly [number, number] | null;
  /** Where bytes of a table go that are sent. */
  readonly offset: number;
}

/** What becomes of it: an answer in its place, or something that goes wrong. */
export type Mishap =
  | { readonly status: number; readonly headers?: Record<string, string> }
  /** The byte at an offset of the table is another. */
  | { readonly damage: number }
  /** Ranges are not known. */
  | "whole"
  /** The connection ends, after so many bytes of what is sent or answered. */
  | { readonly dropAfter: number };

export interface RemoteServer {
  readonly baseUrl: string;
  /** Everything that passed through. */
  readonly passed: Passing[];
  /** Bytes of tables that were answered with. */
  bytes: number;
  mishap: ((passing: Passing) => Mishap | undefined) | null;
  /** Ends the server at once, as a crash would. What it has written stays. */
  kill(): Promise<void>;
  start(): Promise<void>;
  stop(): Promise<void>;
}

const DATABASE =
  process.env.RETICULA_TEST_DATABASE_URL ??
  "postgres://reticula:reticula_local_dev@127.0.0.1:5434/reticula";

async function freePort(): Promise<number> {
  const server = createTcpServer();
  const port = await listen(server);
  await new Promise<void>((closed) => server.close(() => closed()));
  return port;
}

/** A server of its own, with the development sign-in. */
export async function startRemoteServer(): Promise<RemoteServer> {
  const dataDir = await mkdtemp(join(tmpdir(), "reticula-e2e-tables-"));
  const port = await freePort();
  let child: ChildProcess | null = null;
  const logs: string[] = [];
  const passed: Passing[] = [];

  const proxy = createServer((incoming, outgoing) => {
    const { method = "GET", url = "/" } = incoming;
    const [, from, last] = /^bytes=(\d+)-(\d+)$/.exec(incoming.headers.range ?? "") ?? [];
    const passing: Passing = {
      method,
      path: url.replace(/\?.*/, ""),
      range: from === undefined ? null : [Number(from), Number(last) + 1],
      offset: Number(incoming.headers["upload-offset"] ?? 0),
    };
    passed.push(passing);
    pass(incoming, outgoing, passing, handle.mishap?.(passing));
  });
  proxy.on("upgrade", (incoming, socket, head) => {
    const through = connectTo(port, "127.0.0.1", () => {
      const lines = incoming.rawHeaders.flatMap((name, at) =>
        at % 2 === 0 ? [`${name}: ${incoming.rawHeaders[at + 1]}`] : [],
      );
      through.write(`${incoming.method} ${incoming.url} HTTP/1.1\r\n${lines.join("\r\n")}\r\n\r\n`);
      through.write(head);
      through.pipe(socket).pipe(through);
    });
    for (const end of [socket, through]) {
      for (const event of ["error", "close"]) {
        end.on(event, () => {
          socket.destroy();
          through.destroy();
        });
      }
    }
  });

  function pass(
    incoming: IncomingMessage,
    outgoing: ServerResponse,
    { path, range }: Passing,
    mishap: Mishap | undefined,
  ): void {
    if (mishap !== undefined && mishap !== "whole" && "status" in mishap) {
      incoming.resume();
      outgoing.writeHead(mishap.status, mishap.headers).end();
      return;
    }
    const headers = { ...incoming.headers };
    if (mishap === "whole") {
      delete headers.range;
    }
    const onward = request(
      { host: "127.0.0.1", port, method: incoming.method, path: incoming.url, headers },
      (answer) => {
        outgoing.writeHead(answer.statusCode ?? 502, answer.headers);
        let at = mishap === "whole" ? 0 : (range?.[0] ?? 0);
        let left =
          mishap && typeof mishap === "object" && "dropAfter" in mishap
            ? mishap.dropAfter
            : Infinity;
        const isData = path.endsWith("/data") && incoming.method === "GET";
        answer.on("data", (chunk: Buffer) => {
          const part = chunk.subarray(0, left);
          if (mishap && typeof mishap === "object" && "damage" in mishap) {
            const within = mishap.damage - at;
            if (within >= 0 && within < part.length) {
              part.writeUInt8(part.readUInt8(within) ^ 0xff, within);
            }
          }
          at += part.length;
          left -= part.length;
          if (isData) {
            handle.bytes += part.length;
          }
          outgoing.write(part);
          if (left <= 0) {
            outgoing.destroy();
            onward.destroy();
          }
        });
        answer.on("end", () => outgoing.end());
        answer.on("error", () => outgoing.destroy());
      },
    );
    onward.on("error", () => outgoing.destroy());
    outgoing.on("close", () => onward.destroy());
    if (
      incoming.method === "PATCH" &&
      mishap &&
      typeof mishap === "object" &&
      "dropAfter" in mishap
    ) {
      let left = mishap.dropAfter;
      incoming.on("data", (chunk: Buffer) => {
        onward.write(chunk.subarray(0, left));
        left -= chunk.length;
        if (left <= 0) {
          onward.destroy();
          outgoing.destroy();
          incoming.destroy();
        }
      });
      incoming.on("end", () => onward.end());
    } else {
      incoming.pipe(onward);
    }
  }

  const proxyPort = await listen(proxy);
  const baseUrl = `http://127.0.0.1:${proxyPort}`;

  async function start(): Promise<void> {
    const name = process.platform === "win32" ? "reticula-server.exe" : "reticula-server";
    const started = spawn(resolve("target/debug", name), [], {
      env: {
        ...process.env,
        BIND_ADDR: `127.0.0.1:${port}`,
        RETICULA_STATIC_DIR: resolve(process.env.RETICULA_E2E_DIST ?? "apps/web/dist"),
        RETICULA_DATA_DIR: dataDir,
        RETICULA_DATABASE_URL: DATABASE,
        RETICULA_PUBLIC_ORIGIN: baseUrl,
        RETICULA_AUTH_MODE: "dev-mock",
        RETICULA_LLM_MODE: "mock",
      },
      stdio: ["ignore", "pipe", "pipe"],
    });
    child = started;
    started.stdout.on("data", (chunk: Buffer) => logs.push(chunk.toString()));
    started.stderr.on("data", (chunk: Buffer) => logs.push(chunk.toString()));
    const deadline = Date.now() + 20_000;
    for (;;) {
      const isReady = await fetch(`${baseUrl}/readyz`).then(
        ({ ok }) => ok,
        () => false,
      );
      if (isReady) {
        return;
      }
      if (started.exitCode !== null || Date.now() > deadline) {
        throw new Error(`the server did not start:\n${logs.join("")}`);
      }
      await new Promise((later) => setTimeout(later, 100));
    }
  }

  async function kill(): Promise<void> {
    const ending = child;
    child = null;
    if (ending && ending.exitCode === null) {
      await new Promise<void>((ended) => {
        ending.once("exit", () => ended());
        ending.kill("SIGKILL");
      });
    }
  }

  const handle: RemoteServer = {
    baseUrl,
    passed,
    bytes: 0,
    mishap: null,
    kill,
    start,
    async stop() {
      await kill();
      proxy.closeAllConnections();
      await new Promise<void>((closed) => proxy.close(() => closed()));
      await rm(dataDir, { recursive: true, force: true });
    },
  };
  await start();
  return handle;
}

/** Waits for the hooks, which are loaded after everything else. */
export async function hooked(page: Page): Promise<void> {
  await page.waitForFunction(() => window.__reticula !== undefined);
}

/** Signs in without the dialogs, and waits until the page is there. Gives who it is. */
export async function enter(
  page: Page,
  { baseUrl }: RemoteServer,
  account: DevAccount | null,
): Promise<string> {
  let userId = "";
  if (account) {
    const answer = await page.context().request.post(`${baseUrl}/api/auth/dev-login`, {
      headers: { origin: baseUrl },
      data: { account },
    });
    expect(answer.status()).toBe(200);
    userId = ((await answer.json()) as { id: string }).id;
  }
  // A workbook of its own, which no other tab has open.
  await page.goto(`${baseUrl}/app?hooks&doc=source-${randomUUID()}`);
  await expect(page.getByTestId("engine-status")).toHaveText("ready");
  await hooked(page);
  return userId;
}

/** Asks the server for something, as whoever is signed in. */
export async function api<T>(
  page: Page,
  { baseUrl }: RemoteServer,
  method: string,
  path: string,
  data?: unknown,
): Promise<T> {
  const answer = await page.context().request.fetch(`${baseUrl}/api${path}`, {
    method,
    headers: { origin: baseUrl },
    ...(data === undefined ? {} : { data }),
  });
  expect(answer.ok(), `${method} ${path}: ${answer.status()}`).toBe(true);
  const text = await answer.text();
  return (text === "" ? null : JSON.parse(text)) as T;
}

interface Harness {
  client: Client;
  readonly pages: ArrayBuffer[];
  readonly parts: { have: number; total: number; isStalled: boolean }[];
  readonly failures: { code: string }[];
  readonly uploads: { state: string; sent: number; failure?: { code: string } | undefined }[];
}

declare global {
  interface Window {
    __remote?: Harness;
  }
}

export interface Whose {
  readonly docId: string;
  readonly userId: string;
  readonly canWrite: boolean;
  readonly shareToken?: string;
}

/** Starts a client of the tables of a workbook, in place of the one there is. */
export async function connect(page: Page, target: Whose): Promise<TableRecord[]> {
  await hooked(page);
  return page.evaluate(async (whose) => {
    const { TableClient } = await (window.__reticula as NonNullable<Window["__reticula"]>).tables();
    await window.__remote?.client.close();
    const { docId, shareToken } = whose;
    const list = async () => {
      const answer = await fetch(`/api/workbooks/${docId}/tables`, {
        headers: shareToken ? { "X-Reticula-Share": shareToken } : {},
      });
      return answer.ok ? ((await answer.json()).tables as TableRecord[]) : [];
    };
    // What the editor has in its place looks at the list now and then.
    const tables: TableRecord[] = await list().catch(() =>
      JSON.parse(sessionStorage.getItem(docId) ?? "[]"),
    );
    const remote = {
      target: whose,
      tables,
      attach: () => () => {},
      refresh: async () => {},
      look: async () => {},
      giveUp: async () => false,
      change: async (
        edit: (tables: readonly TableRecord[]) => readonly TableRecord[],
        ask: () => Promise<unknown>,
      ) => {
        remote.tables = [...edit(remote.tables)];
        await ask();
        remote.tables = await list();
      },
    };
    sessionStorage.setItem(docId, JSON.stringify(remote.tables));
    const harness: Harness = {
      client: undefined as unknown as Client,
      pages: [],
      parts: [],
      failures: [],
      uploads: [],
    };
    harness.client = new TableClient(
      { docId, tabId: crypto.randomUUID(), remote: remote as never },
      {
        onPage: (buffer) => harness.pages.push(buffer),
        onRemote: (part) => harness.parts.push(part),
        onTableFailed: (failure) => harness.failures.push(failure),
        onFailed: (failure) => harness.failures.push(failure),
        onUpload: (progress) => harness.uploads.push(progress),
      },
    );
    window.__remote = harness;
    await harness.client.acquire();
    return remote.tables;
  }, target);
}

/** What a call of the client comes to: its answer, or why it was refused. */
export async function ask<A, T>(
  page: Page,
  run: (client: Client, argument: A) => Promise<T>,
  argument?: A,
): Promise<T> {
  return page.evaluate(
    ([source, value]) => {
      const call = new Function(`return (${source})`)() as (c: Client, a: A) => Promise<T>;
      return call((window.__remote as Harness).client, value as A);
    },
    [run.toString(), argument] as const,
  );
}

export async function refused<A>(
  page: Page,
  run: (client: Client, argument: A) => Promise<unknown>,
  argument?: A,
): Promise<string> {
  return page.evaluate(
    async ([source, value]) => {
      const call = new Function(`return (${source})`)() as (c: Client, a: A) => Promise<unknown>;
      try {
        await call((window.__remote as Harness).client, value as A);
      } catch (error) {
        return (error as { failure: { code: string } }).failure.code;
      }
      return "it was not refused";
    },
    [run.toString(), argument] as const,
  );
}

/** Makes a table up, and has the server have it, without the worker that uploads. */
export async function putTable(page: Page, docId: string, rows: number): Promise<TableRecord> {
  return page.evaluate(
    async ([id, count]) => {
      const { TableClient } = await (
        window.__reticula as NonNullable<Window["__reticula"]>
      ).tables();
      const local = new TableClient({ docId: `made-${id}`, tabId: crypto.randomUUID() }, {});
      await local.acquire();
      const { entry } = await local.sample("orders", count, 7);
      const file = await local.export(entry.id);
      const bytes = await file.arrayBuffer();
      await local.delete(entry.id);
      await local.close();
      const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
      const sha256 = Array.from(digest, (byte) => byte.toString(16).padStart(2, "0")).join("");
      const url = `/api/workbooks/${id}/tables`;
      const send = async (path: string, method: string, headers: HeadersInit, body?: BodyInit) => {
        const answer = await fetch(url + path, { method, headers, body: body ?? null });
        if (!answer.ok) {
          throw new Error(`${method} ${path}: ${answer.status} ${await answer.text()}`);
        }
        return answer.json();
      };
      const table = crypto.randomUUID();
      await send(
        "",
        "POST",
        { "Content-Type": "application/json" },
        JSON.stringify({ id: table, name: "orders", bytes: bytes.byteLength, sha256 }),
      );
      for (let at = 0; at < bytes.byteLength; at += 8 << 20) {
        await send(
          `/${table}/data`,
          "PATCH",
          { "Upload-Offset": `${at}` },
          bytes.slice(at, at + (8 << 20)),
        );
      }
      return send(`/${table}/complete`, "POST", {});
    },
    [docId, rows] as const,
  );
}

/** The files of a directory in this browser, and of those in it. */
function files(page: Page, ...path: string[]): Promise<string[]> {
  return page.evaluate(async (names) => {
    const found: string[] = [];
    const list = async (dir: FileSystemDirectoryHandle): Promise<void> => {
      const values = (dir as unknown as { values(): AsyncIterable<FileSystemHandle> }).values();
      for await (const handle of values) {
        if (handle.kind === "directory") {
          await list(handle as FileSystemDirectoryHandle);
        } else {
          found.push(handle.name);
        }
      }
    };
    try {
      let dir = await navigator.storage.getDirectory();
      for (const name of names) {
        dir = await dir.getDirectoryHandle(name);
      }
      await list(dir);
    } catch {
      // There is none.
    }
    return found.sort();
  }, path);
}

/** The files with copies of tables in this browser. */
export async function copies(page: Page): Promise<string[]> {
  const found = await files(page, "reticula-v1", "tables-remote");
  return found.filter((name) => !name.endsWith(".json"));
}

/** What there is of what is to be uploaded. */
export function queue(page: Page): Promise<string[]> {
  return files(page, "reticula-v1", "tables-queue");
}

/** Gives up what is to be uploaded. */
export function empty(page: Page): Promise<void> {
  return page.evaluate(async () => {
    const opfs = await navigator.storage.getDirectory();
    const root = await opfs.getDirectoryHandle("reticula-v1");
    await root.removeEntry("tables-queue", { recursive: true });
  });
}
