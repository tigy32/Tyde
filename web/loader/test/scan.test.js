// Tests for the iOS-friendly QR pairing additions:
//   1. the bundled jsQR fallback decode path (web/loader/vendor/jsqr.js),
//   2. `isStandaloneContext` (installed-PWA detection), and
//   3. the Safari-guard routing: auto-pair when standalone, show the guard
//      (and let "Pair here anyway" proceed) when not.
//
// As in handoff.test.js we stub the minimal browser globals BEFORE importing
// loader.js and leave `document` undefined so the module does not auto-run
// init(). The DOM-touching loader helpers all guard on `typeof document` /
// presence of `ui` entries, so the exported logic runs headless.

import { test } from "node:test";
import assert from "node:assert/strict";

// --- jsQR load (UMD) --------------------------------------------------------
// The vendored jsQR is a UMD bundle. Loaded as an ES module here (web/loader is
// `"type": "module"`) it takes the `root["jsQR"] = factory()` branch where
// `root` is `self`; shim `self` to the global so it installs `globalThis.jsQR`,
// mirroring how the browser <script> exposes the global the loader reads.
globalThis.self = globalThis;
await import("../vendor/jsqr.js");

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
  decodeQrImageData,
  isPairingScanValue,
  isStandaloneContext,
  routePairingFragment,
  PAIR_URI_KEY,
} = await import("../loader.js");
const {
  EXAMPLE_MANIFEST,
  REAL_WITH_PRERELEASE,
  SCAN_QR_VALUE,
  SCAN_QR_MATRIX,
  qrMatrixToImageData,
} = await import("./fixtures.js");

// --- 1. jsQR fallback decode path ------------------------------------------

test("decodeQrImageData decodes a real QR ImageData via the bundled jsQR", () => {
  const imageData = qrMatrixToImageData(SCAN_QR_MATRIX);
  // Uses the global jsQR the vendor bundle installed (the browser path).
  assert.equal(decodeQrImageData(imageData), SCAN_QR_VALUE);
});

test("decodeQrImageData returns null on a blank (no-QR) frame", () => {
  const blank = {
    data: new Uint8ClampedArray(40 * 40 * 4).fill(255),
    width: 40,
    height: 40,
  };
  assert.equal(decodeQrImageData(blank), null);
});

test("decodeQrImageData fails closed without a decoder or image", () => {
  // No decoder available → null, never a throw.
  assert.equal(decodeQrImageData({ data: [], width: 1, height: 1 }, null), null);
  assert.equal(decodeQrImageData(null), null);
});

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
