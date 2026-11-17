// Serves the production build (apps/web/dist) the way a deployment would:
// files compressed ahead of time, chosen by what the browser accepts, and
// cached for good where their names carry a hash of their content.
//
// It can also stand in for a network: `network` holds every response back by
// a round trip and lets the bytes of all responses together through at a
// given rate. That is cruder than a real link (there is no slow start and no
// loss), but it applies to every request alike, whichever thread of the page
// made it and whichever browser is asking.

import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { dirname, extname, join, normalize, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/** The repository root. */
export const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");

const types = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript",
  ".css": "text/css",
  ".wasm": "application/wasm",
};
const suffixes = { br: ".br", gzip: ".gz" };
/** Bytes handed to the link at a time: about what fits in a packet. */
const SEGMENT = 1460;

/**
 * Resolves at `time`, on the clock of performance.now(), and not a
 * millisecond late: a timer alone can be late by a tick of the system's
 * clock, which on Windows is long compared with what is timed here. So the
 * timer does the bulk of the wait, and the rest is spent letting other work
 * run and looking at the clock.
 */
async function until(time) {
  for (;;) {
    const left = time - performance.now();
    if (left <= 0) {
      return;
    }
    await new Promise((done) => (left > 20 ? setTimeout(done, left - 20) : setImmediate(done)));
  }
}

/**
 * Starts the server. Resolves to its address, the requests it has answered,
 * and functions that tell it a new visit begins and that stop it.
 *
 *   site       the directory to serve (default: apps/web/dist)
 *   encodings  the compressed forms to offer, best first; [] serves the files as they are
 *   network    { roundTripMs, downBitsPerSecond, connectRoundTrips } or nothing
 */
export async function serve({ port = 0, site = join(root, "apps/web/dist"), encodings = ["br", "gzip"], network = null } = {}) {
  const log = [];
  /** When the link is next free, on the clock of performance.now(). */
  let linkFreeAt = 0;
  /** Whether the visit under way has set its connection up. */
  let connected = false;

  const server = createServer(async (request, response) => {
    const arrived = performance.now();
    const path = new URL(request.url, "http://localhost").pathname;
    const file = normalize(join(site, path === "/" ? "index.html" : path));
    const accepted = String(request.headers["accept-encoding"] ?? "");
    let body;
    let encoding = "";
    try {
      if (!file.startsWith(site)) {
        throw new Error("outside the site");
      }
      for (const candidate of encodings) {
        if (accepted.includes(candidate === "gzip" ? "gzip" : candidate)) {
          try {
            body = await readFile(file + suffixes[candidate]);
            encoding = candidate;
            break;
          } catch {
            // No such form of this file; try the next.
          }
        }
      }
      body ??= await readFile(file);
    } catch {
      response.writeHead(path === "/favicon.ico" ? 204 : 404).end();
      return;
    }

    const headers = {
      "content-type": types[extname(file)] ?? "application/octet-stream",
      "content-length": body.length,
      vary: "accept-encoding",
      // Files under assets/ have a hash of their content in their names.
      "cache-control": path.startsWith("/assets/") ? "public, max-age=31536000, immutable" : "no-cache",
    };
    if (encoding) {
      headers["content-encoding"] = encoding;
    }
    log.push({ path, encoding: encoding || "identity", bytes: body.length });

    if (!network) {
      response.writeHead(200, headers).end(body);
      return;
    }
    // The request took half a round trip to get here and the answer takes
    // the other half. A visit's first request also pays for setting up the
    // connection, once: a deployment speaks HTTP/2, where every request of
    // the visit shares one connection, whatever this server's own
    // connections with the browser are.
    let wait = network.roundTripMs;
    if (!connected) {
      connected = true;
      wait += network.roundTripMs * (network.connectRoundTrips ?? 0);
    }
    await until(arrived + wait);
    response.writeHead(200, headers);
    const bytesPerMs = network.downBitsPerSecond / 8 / 1000;
    // Segments of all responses take turns on the link. A segment is through
    // once the link has carried all of it.
    let ready = performance.now();
    for (let sent = 0; sent < body.length; sent += SEGMENT) {
      const segment = body.subarray(sent, sent + SEGMENT);
      linkFreeAt = Math.max(ready, linkFreeAt) + segment.length / bytesPerMs;
      ready = linkFreeAt;
      await until(ready);
      response.write(segment);
    }
    response.end();
  });
  await new Promise((ready) => server.listen(port, "127.0.0.1", ready));
  return {
    address: `http://localhost:${server.address().port}`,
    log,
    /** Tells the server that the next request begins a new visit. */
    newVisit() {
      connected = false;
    },
    close: () => server.close(),
  };
}
