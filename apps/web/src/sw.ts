/// Hand-written Service Worker offline shell (`14-m1b.md` 7.4 & D-019).
///
/// Build-time manifest injection replaces `__RETICULA_BUILD_ID__`,
/// `["__RETICULA_PRECACHE__"]` and `["__RETICULA_ON_USE__"]` in `dist/sw.js`
/// before `.br`/`.gz` precompression.
///
/// Update policy:
/// - New versions install into their own `reticula-shell-<BUILD_ID>` cache and
///   stay in `waiting` until a client coordinates OPFS drain across all open
///   tabs and posts `{ kind: "SKIP_WAITING" }`.
/// - Hashed `/assets/*` are served cache-first; only a response of the kind the
///   name asks for is kept.
/// - What not everybody needs is kept when it is first used, in a cache that
///   outlives an update: a new version adds what it has of it before it takes
///   over, and deletes what it has no use for. Pages that were loaded before
///   there was a worker say what they have in use.
/// - Navigation / HTML requests are served network-first with instant fallback
///   to the precached `/index.html` shell when offline.

import { isAssetResponse } from "./sw-assets.ts";

interface ExtendableEventLike extends Event {
  waitUntil(promise: Promise<unknown>): void;
}

interface FetchEventLike extends ExtendableEventLike {
  readonly request: Request;
  respondWith(response: Response | Promise<Response>): void;
}

interface MessageSourceLike {
  postMessage(message: unknown, transfer?: Transferable[]): void;
}

interface ExtendableMessageEventLike extends ExtendableEventLike {
  readonly data: unknown;
  readonly ports: readonly MessagePort[];
  readonly source: MessageSourceLike | null;
}

interface ServiceWorkerClientsLike {
  claim(): Promise<void>;
}

interface ServiceWorkerGlobalLike {
  readonly location: Location;
  readonly clients: ServiceWorkerClientsLike;
  skipWaiting(): Promise<void>;
  addEventListener(type: "install", listener: (event: ExtendableEventLike) => void): void;
  addEventListener(type: "activate", listener: (event: ExtendableEventLike) => void): void;
  addEventListener(type: "fetch", listener: (event: FetchEventLike) => void): void;
  addEventListener(type: "message", listener: (event: ExtendableMessageEventLike) => void): void;
}

const sw = globalThis as unknown as ServiceWorkerGlobalLike;

const BUILD_ID = "__RETICULA_BUILD_ID__";
const PRECACHE_URLS: readonly string[] = ["__RETICULA_PRECACHE__"];
const CACHE_PREFIX = "reticula-shell-";
const CACHE_NAME = `${CACHE_PREFIX}${BUILD_ID}`;
const ON_USE_URLS: readonly string[] = ["__RETICULA_ON_USE__"];
const ON_USE_CACHE = "reticula-optional";
const NAV_TIMEOUT_MS = 3_000;

function isUsable(pathname: string, response: Response): boolean {
  return (
    !pathname.startsWith("/assets/") ||
    isAssetResponse(pathname, response.headers.get("content-type"))
  );
}

async function precache(
  name: string,
  urls: readonly string[],
  mode: RequestCache = "reload",
): Promise<void> {
  const cache = await caches.open(name);
  await Promise.all(
    urls.map(async (url) => {
      if (mode !== "reload" && (await cache.match(url))) {
        return;
      }
      const request = new Request(url, { cache: mode });
      const response = await fetch(request);
      if (!response.ok || !isUsable(url, response)) {
        const type = response.headers.get("content-type");
        throw new Error(`Precache fetch failed for ${url}: HTTP ${response.status}, ${type}`);
      }
      await cache.put(url, response);
    }),
  );
}

async function install(): Promise<void> {
  await precache(
    CACHE_NAME,
    PRECACHE_URLS.filter((url) => url.startsWith("/")),
  );
  // Who has used the rest will look for it without a connection as well.
  if (await caches.has(ON_USE_CACHE)) {
    const used = await (await caches.open(ON_USE_CACHE)).keys();
    if (used.length > 0) {
      await precache(ON_USE_CACHE, ON_USE_URLS, "default");
    }
  }
}

async function cleanupOldCaches(): Promise<void> {
  const keys = await caches.keys();
  await Promise.all(
    keys
      .filter((key) => key.startsWith(CACHE_PREFIX) && key !== CACHE_NAME)
      .map((key) => caches.delete(key)),
  );
  if (await caches.has(ON_USE_CACHE)) {
    const cache = await caches.open(ON_USE_CACHE);
    const requests = await cache.keys();
    await Promise.all(
      requests
        .filter(({ url }) => !ON_USE_URLS.includes(new URL(url).pathname))
        .map((request) => cache.delete(request)),
    );
  }
  await sw.clients.claim();
}

async function cacheFirstAsset(request: Request): Promise<Response> {
  const { pathname } = new URL(request.url);
  const cache = await caches.open(ON_USE_URLS.includes(pathname) ? ON_USE_CACHE : CACHE_NAME);
  const hit = (await cache.match(request)) ?? (await caches.match(request));
  // An older version of this worker may have kept a response it should not have.
  if (hit && isUsable(pathname, hit)) {
    return hit;
  }
  const response = await fetch(request);
  if (response.status === 200 && isUsable(pathname, response)) {
    await cache.put(request, response.clone());
  }
  return response;
}

async function networkFirstHtml(request: Request): Promise<Response> {
  const cache = await caches.open(CACHE_NAME);
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), NAV_TIMEOUT_MS);
  try {
    const response = await fetch(request, { signal: controller.signal });
    if (response.ok && response.status === 200) {
      return response;
    }
  } catch {
    // Offline or network timeout — serve precached SPA shell below.
  } finally {
    clearTimeout(timer);
  }
  const cached =
    (await cache.match("/index.html")) ??
    (await caches.match("/index.html")) ??
    (await cache.match(request));
  if (cached) {
    return cached;
  }
  return new Response("Reticula is offline and the cached shell is not available yet.", {
    status: 503,
    headers: { "content-type": "text/plain; charset=utf-8" },
  });
}

sw.addEventListener("install", (event) => {
  // Never call `skipWaiting()` automatically here so active tabs can drain OPFS
  // handles cleanly when the user confirms the update (`14-m1b.md` 7.4).
  event.waitUntil(install());
});

sw.addEventListener("activate", (event) => {
  event.waitUntil(cleanupOldCaches());
});

sw.addEventListener("message", (event) => {
  const data = event.data as { kind?: string; paths?: readonly string[] } | null;
  if (!data || typeof data !== "object") {
    return;
  }
  if (data.kind === "SKIP_WAITING") {
    event.waitUntil(sw.skipWaiting());
  } else if (data.kind === "IN_USE") {
    if (data.paths?.some((path) => ON_USE_URLS.includes(path))) {
      event.waitUntil(precache(ON_USE_CACHE, ON_USE_URLS, "default").catch(() => {}));
    }
  } else if (data.kind === "GET_VERSION") {
    const reply = { kind: "VERSION", buildId: BUILD_ID };
    const port = event.ports[0];
    if (port) {
      port.postMessage(reply);
    } else {
      event.source?.postMessage(reply);
    }
  }
});

sw.addEventListener("fetch", (event) => {
  const { request } = event;
  if (request.method !== "GET") {
    return;
  }
  const url = new URL(request.url);
  if (url.origin !== sw.location.origin) {
    return;
  }
  if (
    url.pathname === "/sw.js" ||
    url.pathname === "/healthz" ||
    url.pathname.startsWith("/api/")
  ) {
    return;
  }

  if (url.pathname.startsWith("/assets/")) {
    event.respondWith(cacheFirstAsset(request));
    return;
  }

  if (
    request.mode === "navigate" ||
    url.pathname === "/" ||
    url.pathname === "/index.html" ||
    url.pathname === "/app" ||
    url.pathname.startsWith("/app/") ||
    url.pathname.startsWith("/join/")
  ) {
    event.respondWith(networkFirstHtml(request));
  }
});
