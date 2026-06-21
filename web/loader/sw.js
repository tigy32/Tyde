// Tyde loader service worker.
//
// Two caching regimes:
//   1. Loader shell + manifest.json  -> network-first (fall back to cache).
//      The shell and the version allowlist must never get stuck on stale logic,
//      so we always try the network first and only serve the cache offline.
//   2. Versioned bundles `/tyde/v<version>/...` -> cache-first. These paths are
//      immutable (the version is in the path), so once cached they can be
//      served forever without revalidation, giving offline launch of an
//      already-paired client.
//
// Bump LOADER_CACHE when the precache list or shell logic changes.

const LOADER_CACHE = "tyde-loader-v1";
const BUNDLE_CACHE = "tyde-bundle-v1";

// Paths are relative to the SW scope (`/tyde/`).
const SHELL_ASSETS = [
  "./",
  "./index.html",
  "./loader.js",
  "./loader.css",
  "./cbor.js",
  "./pairing.js",
  "./manifest-policy.js",
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

  if (isVersionedBundle(url)) {
    event.respondWith(cacheFirst(request, BUNDLE_CACHE));
    return;
  }

  // Loader shell (including navigations) and the manifest: network-first.
  if (isManifest(url) || request.mode === "navigate" || isShellAsset(url)) {
    event.respondWith(networkFirst(request, LOADER_CACHE));
  }
});

function isShellAsset(url) {
  return /\/(index\.html|loader\.(js|css)|cbor\.js|pairing\.js|manifest-policy\.js|manifest\.webmanifest)$/.test(
    url.pathname,
  ) || url.pathname.endsWith("/tyde/") || url.pathname.endsWith("/");
}

async function cacheFirst(request, cacheName) {
  const cache = await caches.open(cacheName);
  const cached = await cache.match(request);
  if (cached) return cached;
  const response = await fetch(request);
  if (response.ok) cache.put(request, response.clone());
  return response;
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
