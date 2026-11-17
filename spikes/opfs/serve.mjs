// Serves the experiment page (spikes/opfs/page) as it is: there is no build.

import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { dirname, extname, join, normalize, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
/** The repository root. */
export const root = resolve(here, "../..");

const site = join(here, "page");
const types = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript",
};

/**
 * Starts the server on a port of the system's choosing, or on `port`.
 * Resolves to its address and a function that stops it.
 */
export async function serve(port = 0) {
  const server = createServer(async (request, response) => {
    const path = new URL(request.url, "http://localhost").pathname;
    const file = normalize(join(site, path === "/" ? "index.html" : path));
    let body;
    try {
      if (!file.startsWith(site)) {
        throw new Error("outside the site");
      }
      body = await readFile(file);
    } catch {
      response.writeHead(path === "/favicon.ico" ? 204 : 404).end();
      return;
    }
    response.writeHead(200, {
      "content-type": types[extname(file)] ?? "application/octet-stream",
      "cache-control": "no-store",
      // Cross-origin isolation: browsers give an isolated page a finer clock,
      // which the timings of writes and flushes need.
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
