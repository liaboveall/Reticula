// Serves pages to pages that measure: the ones built by `vite build --mode
// spikes` (apps/web/dist-spikes), unless another build is named.

import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/** The repository root. */
export const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");

const types = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript",
  ".css": "text/css",
  ".wasm": "application/wasm",
  ".json": "application/json",
  ".svg": "image/svg+xml",
};

/**
 * Starts the server on a port of the system's choosing, or on `port`, with
 * the files of `site`. Paths that name no file get the `document`, if there is
 * one: the app has many addresses. Resolves to the server's address and a
 * function that stops it.
 */
export async function serve(port = 0, { site = join(root, "apps/web/dist-spikes"), document = "" } = {}) {
  const server = createServer(async (request, response) => {
    const { pathname } = new URL(request.url, "http://localhost");
    const path = document && !extname(pathname) ? `/${document}` : pathname;
    if (path === "/favicon.ico") {
      response.writeHead(204).end();
      return;
    }
    let body;
    try {
      body = await readFile(join(site, path));
    } catch {
      response.writeHead(404).end();
      return;
    }
    response.writeHead(200, {
      "content-type": types[extname(path)] ?? "application/octet-stream",
      "cache-control": "no-store",
      // Cross-origin isolation: browsers give an isolated page a finer clock.
      "cross-origin-opener-policy": "same-origin",
      "cross-origin-embedder-policy": "require-corp",
    });
    response.end(body);
  });
  await new Promise((ready) => server.listen(port, "127.0.0.1", ready));
  return {
    address: `http://localhost:${server.address().port}`,
    close: () => server.close(),
  };
}
