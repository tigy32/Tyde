// #3 — the loader must hand the full raw pairing URI to the booted WASM app so
// first-time pairing can complete. It stashes the URI in sessionStorage under
// PAIR_URI_KEY (consumed by mobile-frontend's bridge::web::take_pending_pairing_uri).
//
// We stub the minimal browser globals and call the exported handlePairingUri
// directly. `document` is intentionally left undefined so loader.js does NOT
// auto-run init() on import.

import { test } from "node:test";
import assert from "node:assert/strict";

// Install stubs BEFORE importing loader.js.
const sessionStore = {};
const localStore = {};
globalThis.sessionStorage = {
  getItem: (k) => (k in sessionStore ? sessionStore[k] : null),
  setItem: (k, v) => {
    sessionStore[k] = String(v);
  },
  removeItem: (k) => {
    delete sessionStore[k];
  },
};
globalThis.localStorage = {
  getItem: (k) => (k in localStore ? localStore[k] : null),
  setItem: (k, v) => {
    localStore[k] = String(v);
  },
  removeItem: (k) => {
    delete localStore[k];
  },
};
// Artifact fetches return bytes that will NOT match the placeholder manifest
// hashes, so verification fails and boot aborts — AFTER the URI is stashed,
// which is exactly the behavior under test.
globalThis.fetch = async () => ({
  ok: true,
  clone() {
    return this;
  },
  async arrayBuffer() {
    return new TextEncoder().encode("not-the-real-bundle").buffer;
  },
});

const { handlePairingUri, PAIR_URI_KEY } = await import("../loader.js");
const { REAL_WITH_PRERELEASE, EXAMPLE_MANIFEST, makePairingUri } = await import(
  "./fixtures.js"
);

test("handlePairingUri stashes the raw URI for the app on the pair path", async () => {
  delete sessionStore[PAIR_URI_KEY];
  await handlePairingUri(REAL_WITH_PRERELEASE, EXAMPLE_MANIFEST);
  assert.equal(sessionStore[PAIR_URI_KEY], REAL_WITH_PRERELEASE);
});

test("handlePairingUri does NOT stash when the URI is not a pairing code", async () => {
  delete sessionStore[PAIR_URI_KEY];
  await handlePairingUri("https://evil.example/", EXAMPLE_MANIFEST);
  assert.equal(sessionStore[PAIR_URI_KEY], undefined);
});

test("handlePairingUri does NOT stash when the version is not in the manifest", async () => {
  delete sessionStore[PAIR_URI_KEY];
  // A synthetic URI whose release_version is a valid semver absent from the manifest.
  const uri = makePairingUri({ v: 2, protocol_version: 13, release_version: "99.99.99" });
  await handlePairingUri(uri, EXAMPLE_MANIFEST);
  assert.equal(sessionStore[PAIR_URI_KEY], undefined);
});
