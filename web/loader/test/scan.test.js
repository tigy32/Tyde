// Loader pairing policy; real DOM/pixel decoding lives in browser-flow.test.js.

import { test } from "node:test";
import assert from "node:assert/strict";

// --- browser-global stubs (install before importing loader.js) --------------
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
// Artifact fetches return bytes that never match the placeholder manifest
// hashes, so a boot aborts at SRI verification — AFTER the URI is stashed,
// which is the observable signal that pairing proceeded.
globalThis.fetch = async () => ({
  ok: true,
  clone() {
    return this;
  },
  async arrayBuffer() {
    return new TextEncoder().encode("not-the-real-bundle").buffer;
  },
});

const {
  isPairingScanValue,
  isStandaloneContext,
  routePairingFragment,
  PAIR_URI_KEY,
} = await import("../loader.js");
const {
  EXAMPLE_MANIFEST,
  REAL_WITH_PRERELEASE,
} = await import("./fixtures.js");

test("isPairingScanValue accepts raw and HTTPS-fragment pairing forms", () => {
  assert.equal(isPairingScanValue("tyde-pair://v1?abc"), true);
  assert.equal(isPairingScanValue("tyde-pair://v2?abc"), true);
  assert.equal(isPairingScanValue("https://tycode.dev/tyde/#tyde-pair://v1?abc"), true);
  assert.equal(isPairingScanValue("https://tycode.dev/tyde/#tyde-pair://v2?abc"), true);
  assert.equal(isPairingScanValue("https://example.com/"), false);
  assert.equal(isPairingScanValue(""), false);
  assert.equal(isPairingScanValue(undefined), false);
});

// --- 2. installed-PWA / standalone detection -------------------------------

test("isStandaloneContext detects iOS Add-to-Home-Screen (navigator.standalone)", () => {
  assert.equal(isStandaloneContext({}, { standalone: true }), true);
});

test("isStandaloneContext detects the standalone display-mode", () => {
  const win = {
    matchMedia: (q) => ({ matches: q === "(display-mode: standalone)" }),
  };
  assert.equal(isStandaloneContext(win, {}), true);
});

test("isStandaloneContext is false in a normal browser tab / in-app preview", () => {
  const win = { matchMedia: () => ({ matches: false }) };
  assert.equal(isStandaloneContext(win, { standalone: false }), false);
  // Missing globals (no matchMedia, no standalone) → not standalone, no throw.
  assert.equal(isStandaloneContext({}, {}), false);
  assert.equal(isStandaloneContext(undefined, undefined), false);
});

// --- 3. Safari-guard routing -----------------------------------------------

test("routePairingFragment auto-pairs when standalone/installed", async () => {
  delete sessionStore[PAIR_URI_KEY];
  // standalone = true → pair immediately; the URI is stashed for the app.
  await routePairingFragment(REAL_WITH_PRERELEASE, EXAMPLE_MANIFEST, true);
  assert.equal(sessionStore[PAIR_URI_KEY], REAL_WITH_PRERELEASE);
});

test("routePairingFragment shows the guard (no auto-pair) when NOT standalone", async () => {
  delete sessionStore[PAIR_URI_KEY];
  // standalone = false → guard screen, the URI must NOT be auto-stashed.
  await routePairingFragment(REAL_WITH_PRERELEASE, EXAMPLE_MANIFEST, false);
  assert.equal(sessionStore[PAIR_URI_KEY], undefined);
});

test("routePairingFragment normalizes the HTTPS-fragment QR form when pairing", async () => {
  delete sessionStore[PAIR_URI_KEY];
  const httpsForm = `https://tycode.dev/tyde/#${REAL_WITH_PRERELEASE}`;
  await routePairingFragment(httpsForm, EXAMPLE_MANIFEST, true);
  // The app's bridge expects the inner tyde-pair:// URI, not the wrapping URL.
  assert.equal(sessionStore[PAIR_URI_KEY], REAL_WITH_PRERELEASE);
});

test('guard "Pair here anyway" proceeds via the normal pairing path', async () => {
  // The guard keeps the URI in memory and wires a "Pair here anyway" button to
  // the SAME handlePairingUri the standalone path uses. Driving that path with
  // the held URI must stash it for the app — proving the user is never blocked.
  delete sessionStore[PAIR_URI_KEY];
  assert.equal(sessionStore[PAIR_URI_KEY], undefined); // guard shown, not paired
  // User taps "Pair here anyway" → same routing with the held URI, now proceeding.
  await routePairingFragment(REAL_WITH_PRERELEASE, EXAMPLE_MANIFEST, true);
  assert.equal(sessionStore[PAIR_URI_KEY], REAL_WITH_PRERELEASE);
});
