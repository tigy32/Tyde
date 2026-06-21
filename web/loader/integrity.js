// Subresource-integrity verification for ALL executable artifacts of a version
// (entry JS + wasm + code-split chunks), not just the entry `<script>`.
//
// WHY: a `<script integrity>` tag only covers the entry JS. The Trunk-generated
// entry then fetches its `.wasm` (and any chunks) WITHOUT integrity, so a
// tampered same-version `.wasm` could execute. This module closes that gap: the
// loader fetches every artifact, computes its digest with WebCrypto, and
// compares it to the manifest hash BEFORE the bundle runs. On a match the
// verified Response is written into the bundle cache, so the subsequent
// `<script>`/wasm load reads the exact verified bytes (single network fetch).
//
// Pure + injectable (`fetchImpl`, `subtle`, `cache`) so it is unit-testable
// under `node --test` with `globalThis.crypto.subtle` and no real network.

const HASH_ALGO = { 256: "SHA-256", 384: "SHA-384", 512: "SHA-512" };

function bytesToBase64(bytes) {
  let binary = "";
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode(...bytes.subarray(i, i + chunk));
  }
  return btoa(binary);
}

// Verifies a single artifact. Returns true on a digest match. When `cache` is
// provided, the verified Response is stored under `url` on success.
async function verifyOne(artifact, fetchImpl, subtle, cache) {
  const m = /^sha(256|384|512)-(.+)$/.exec(artifact.integrity);
  if (!m) return false;
  const algo = HASH_ALGO[m[1]];
  const expected = m[2];

  // `no-store`: bypass any cache so we hash exactly what the server returns now,
  // and never trust a possibly-poisoned cached copy during verification.
  const response = await fetchImpl(artifact.url, { cache: "no-store" });
  if (!response || !response.ok) return false;

  // Clone before consuming the body so the verified bytes can be cached intact.
  const toCache = cache ? response.clone() : null;
  const buffer = await response.arrayBuffer();
  const digest = await subtle.digest(algo, buffer);
  const actual = bytesToBase64(new Uint8Array(digest));
  if (actual !== expected) return false;

  if (cache && toCache) {
    try {
      await cache.put(artifact.url, toCache);
    } catch {
      // Caching is an optimization; a failure to persist must not fail the
      // boot (the bytes were already verified in-memory).
    }
  }
  return true;
}

// Verifies every artifact. Resolves `{ ok, failures }` where `failures` lists
// the URLs that did not match. Caching happens only for artifacts that verify,
// so a tampered 200 is never persisted (#5a — no cache-before-verify).
export async function verifyArtifacts(artifacts, options = {}) {
  const fetchImpl = options.fetchImpl || globalThis.fetch;
  const subtle =
    options.subtle || (globalThis.crypto && globalThis.crypto.subtle);
  const cache = options.cache || null;
  if (typeof fetchImpl !== "function" || !subtle) {
    return { ok: false, failures: artifacts.map((a) => a.url) };
  }

  const failures = [];
  for (const artifact of artifacts) {
    let ok = false;
    try {
      ok = await verifyOne(artifact, fetchImpl, subtle, cache);
    } catch {
      ok = false;
    }
    if (!ok) failures.push(artifact.url);
  }
  return { ok: failures.length === 0, failures };
}
