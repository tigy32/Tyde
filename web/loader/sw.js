// Tyde loader service worker.
//
// Caching regimes:
//   1. Loader shell  -> network-first (fall back to cache). The shell must
//      never get stuck on stale logic, so we try the network first and only
//      serve the cache when offline.
//   2. `manifest.json` -> NETWORK-ONLY (never cached, never served from cache).
//      The manifest is the security authority (allowlist + `blocked`/
//      `minSupported` revocations). Serving a stale copy would let an attacker
//      defeat a revocation by forcing an outage, so a manifest fetch FAILS
//      CLOSED when offline rather than falling back to a cached allowlist.
//   3. Versioned bundles `/tyde/v<version>/...` -> cache-first READ, but this SW
//      NEVER writes them on a plain fetch. The only writer is the loader page,
//      and only AFTER it has SRI-verified the bytes (see integrity.js). So a
//      tampered 200 fetched here is passed through to the page (which verifies)
//      but never persisted — no cache-before-verify, no poisoning.
//
// Bump LOADER_CACHE when the precache list or shell logic changes.

const LOADER_CACHE = "tyde-loader-v2";
const BUNDLE_CACHE = "tyde-bundle-v1"; // shared with loader.js

// Paths are relative to the SW scope (`/tyde/`).
const SHELL_ASSETS = [
  "./",
  "./index.html",
  "./loader.js",
  "./loader.css",
  "./cbor.js",
  "./pairing.js",
  "./manifest-policy.js",
  "./integrity.js",
  "./manifest.webmanifest",
  "./icons/icon.svg",
];

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches.open(LOADER_CACHE).then((cache) => cache.addAll(SHELL_ASSETS)),
  );
  self.skipWaiting();
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) =>
        Promise.all(
          keys
            .filter((key) => key !== LOADER_CACHE && key !== BUNDLE_CACHE)
            .map((key) => caches.delete(key)),
        ),
      )
      .then(() => self.clients.claim()),
  );
});

function isVersionedBundle(url) {
  // e.g. /tyde/v0.8.19-beta.2/tyde-mobile.js
  return /\/tyde\/v[^/]+\//.test(url.pathname);
}

function isManifest(url) {
  return url.pathname.endsWith("/manifest.json");
}

self.addEventListener("fetch", (event) => {
  const { request } = event;
  if (request.method !== "GET") return;

  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;

  // Security authority: never cache, never serve from cache. Let the request go
  // to the network untouched so an outage surfaces as a failure (fail closed).
  if (isManifest(url)) return;

  if (isVersionedBundle(url)) {
    event.respondWith(bundleCacheFirst(request));
    return;
  }

  // Loader shell (including navigations): network-first.
  if (request.mode === "navigate" || isShellAsset(url)) {
    event.respondWith(networkFirst(request, LOADER_CACHE));
  }
});

function isShellAsset(url) {
  return (
    /\/(index\.html|loader\.(js|css)|cbor\.js|pairing\.js|manifest-policy\.js|integrity\.js|manifest\.webmanifest)$/.test(
      url.pathname,
    ) || url.pathname.endsWith("/")
  );
}

// Cache-first READ for immutable versioned bundles. On a miss, fetch from the
// network and return it WITHOUT caching — only the page (post SRI-verify) is
// allowed to populate this cache.
async function bundleCacheFirst(request) {
  const cache = await caches.open(BUNDLE_CACHE);
  const cached = await cache.match(request);
  if (cached) return cached;
  return fetch(request);
}

async function networkFirst(request, cacheName) {
  const cache = await caches.open(cacheName);
  try {
    const response = await fetch(request);
    if (response.ok) cache.put(request, response.clone());
    return response;
  } catch (err) {
    const cached = await cache.match(request);
    if (cached) return cached;
    throw err;
  }
}
