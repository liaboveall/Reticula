// Serves the pages built by `vite build --mode spikes` (apps/web/dist-spikes):
// the measurement page and the page for checking input methods.

import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/** The repository root. */
export const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");

const site = join(root, "apps/web/dist-spikes");
const types = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript",
  ".css": "text/css",
  ".wasm": "application/wasm",
};

/**
 * Starts the server on a port of the system's choosing, or on `port`.
 * Resolves to its address and a function that stops it.
 */
export async function serve(port = 0) {
  const server = createServer(async (request, response) => {
    const path = new URL(request.url, "http://localhost").pathname;
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
