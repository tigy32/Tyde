// Tyde web loader — the thin, stable shell served at the origin root
// (`https://tycode.dev/tyde/`). It learns the paired host's release version
// from the pairing QR, validates it, looks it up in the server-controlled
// manifest (the allowlist authority), and boots the matching immutable
// versioned bundle under a strict CSP with Subresource Integrity covering
// EVERY executable artifact (entry JS + wasm + chunks).
//
// This file is the ONLY part of the loader that touches the DOM/network. All
// parsing and policy logic lives in the pure, unit-tested modules it imports.
//
// Keep this file small and rarely-changed: it is the one piece that is NOT
// versioned, so every byte here is permanent surface.

import { parsePairingUri, extractPairingUri } from "./pairing.js";
import {
  resolveBootTarget,
  resolveLatestBootTarget,
  selectBootUrls,
} from "./manifest-policy.js";
import { verifyArtifacts } from "./integrity.js";
import { resolveBundleStylesheets } from "./styles.js";
import { detectScanCapability, pairingUiState } from "./pairing-ui.js";

const MANIFEST_URL = "./manifest.json";
export const STORAGE_KEY = "tyde.loader.version"; // last successfully-paired version
// Handoff contract with mobile-frontend (bridge::web::take_pending_pairing_uri):
// the loader stashes the FULL raw `tyde-pair://…` URI here so the booted WASM
// app can complete first-time pairing. sessionStorage (not the URL) is used so
// the PSK-bearing URI never enters history/referrer; the app reads, validates,
// and clears it. MUST match `PENDING_PAIRING_URI_KEY` in the Rust web bridge.
export const PAIR_URI_KEY = "tyde.pair.uri";
// Self-heal handoff: a stale booted bundle that hit a protocol mismatch during
// in-app pairing dispatches `tyde:repair-needed` carrying the host's raw pairing
// URI. The loader stashes it here, reloads (tearing down the stale WASM), and
// the fresh `init()` re-routes it through `handlePairingUri` to boot the
// version-matched bundle. sessionStorage (not the URL) keeps the PSK-bearing URI
// out of history/referrer, same as PAIR_URI_KEY.
export const REPAIR_URI_KEY = "tyde.repair.uri";
// Reconnect self-heal handoff: a booted bundle whose COMPILED protocol no longer
// matches an already-paired host (the host was upgraded under it) gets an
// app-level protocol-incompatible reject carrying the host's release version.
// The bundle dispatches `tyde:repair-version` with that version; the loader
// stashes it here and reloads so the fresh `init()` boots the version-matched
// bundle. Unlike REPAIR_URI_KEY this needs no pairing URI — the paired host is
// already in IndexedDB, so the rebooted bundle restores and reconnects to it.
// Shares the REPAIR_ATTEMPTS_KEY loop guard so a misconfigured release cannot
// wedge the PWA in an infinite reload.
export const REPAIR_VERSION_KEY = "tyde.repair.version";
// Session-scoped counter of self-heal reboots already processed, so a corrupt
// manifest that stamps a matching protocol but serves a bundle whose COMPILED
// protocol differs (the booted bundle re-dispatches `tyde:repair-needed`
// forever) can't wedge the PWA in an infinite reload loop. Capped by
// MAX_REPAIR_ATTEMPTS; on exceed the loader clears repair state and shows an
// explicit error instead of reloading again. Session-scoped so closing/
// reopening the PWA — or a clean (non-repair) load — starts fresh.
export const REPAIR_ATTEMPTS_KEY = "tyde.repair.attempts";
export const MAX_REPAIR_ATTEMPTS = 2;
// In-flight pairing fragment captured from the URL. The PSK fragment is cleared
// from the URL IMMEDIATELY (no-leak invariant), but a failure before pairing
// commits — most importantly a failed manifest fetch, whose retry is a full
// `window.location.reload()` — would otherwise lose the QR and force a rescan.
// So we mirror it here in sessionStorage (same PSK-in-sessionStorage exposure as
// PAIR_URI_KEY/REPAIR_URI_KEY, session-scoped, never in the URL) and recover it
// on the next load. Consumed (cleared) once a URI is committed to a boot attempt
// in `handlePairingUri`.
export const PENDING_FRAGMENT_KEY = "tyde.pending.fragment";
const BUNDLE_CACHE = "tyde-bundle-v1"; // shared with sw.js
const WEB_DB_NAME = "tyde-mobile";
const WEB_DB_VERSION = 1;
const WEB_HOSTS_STORE = "paired_hosts";
const WEB_PSK_STORE = "psk";
const WEB_HOSTS_KEY = "all";

const ui = {};

function $(id) {
  return document.getElementById(id);
}

function show(view) {
  for (const name of ["loading", "pair", "pairGuard", "booting", "error"]) {
    const el = ui[name];
    if (el) el.hidden = name !== view;
  }
}

function setError(message, detail) {
  // Make sure the loader chrome is visible to host the error view: a boot that
  // already hid the shell (or a late failure) must un-hide it so the user sees
  // the message instead of a blank page.
  showLoaderShell();
  show("error");
  if (ui.errorMessage) ui.errorMessage.textContent = message;
  if (ui.errorDetail) ui.errorDetail.textContent = detail || "";
}

// The loader chrome (#loader-shell) and the app's mount target (#app-root) are
// SEPARATE containers (see index.html). On a successful boot the app mounts into
// #app-root; we then hide the shell so only the app shows. On failure we keep /
// re-show the shell so the error view is visible.
function hideLoaderShell() {
  if (typeof document === "undefined") return;
  const shell = document.getElementById("loader-shell");
  if (shell) shell.hidden = true;
}

function showLoaderShell() {
  if (typeof document === "undefined") return;
  const shell = document.getElementById("loader-shell");
  if (shell) shell.hidden = false;
}

// Maps a resolveBootTarget failure reason to a user-facing message.
function reasonToMessage(reason) {
  switch (reason) {
    case "blocked":
      return "This host version has been blocked for safety. Update the host, then re-pair.";
    case "below-min-supported":
      return "This host is too old for the current web client. Update the host, then re-pair.";
    case "not-in-manifest":
      return "No matching client build is published for this host version yet. Try again later or re-pair after updating the host.";
    case "protocol-unpublished":
      return "This host build's mobile client has not been published yet. Try again after the release finishes, or report this if it persists.";
    case "protocol-mismatch":
      return "This host does not match its published mobile client (protocol mismatch). The web client needs re-publishing for this host build — please report this.";
    case "repair-loop":
      return "The Tyde client could not start after several attempts — the published release looks misconfigured. Please report this, then update the host and re-pair.";
    case "invalid-version":
      return "The pairing code carried an invalid version. Scan a fresh QR code.";
    case "bad-policy":
    case "bad-entry-path":
    case "bad-integrity":
    case "bad-protocol-version":
      return "The release manifest is malformed. Please report this.";
    case "no-manifest":
      return "Could not load the release manifest. Check your connection and retry.";
    default:
      return "Unable to start the Tyde client.";
  }
}

async function fetchManifest() {
  // Network-only: the manifest is the security authority (allowlist +
  // blocked/minSupported revocations), so it must never be served from a stale
  // cache. The service worker also refuses to cache it (see sw.js) — a forced
  // outage fails closed rather than letting a stale allowlist permit a boot.
  const response = await fetch(MANIFEST_URL, { cache: "no-store" });
  if (!response.ok) {
    throw new Error(`manifest fetch failed: HTTP ${response.status}`);
  }
  return response.json();
}

async function openBundleCache() {
  if (typeof caches === "undefined") return null;
  try {
    return await caches.open(BUNDLE_CACHE);
  } catch {
    return null;
  }
}

async function deleteCachedArtifacts(target) {
  const cache = await openBundleCache();
  if (!cache) return;
  for (const artifact of target.artifacts) {
    try {
      await cache.delete(artifact.url);
    } catch {
      /* ignore */
    }
  }
}

// Boots a resolved target: SRI-verify every executable artifact, then run the
// Trunk-style boot — dynamically `import()` the entry module and call its
// default `init()` with the explicit hashed wasm path. The URLs and integrity
// hashes come entirely from the manifest — never from the QR text — and the page
// CSP (`script-src 'self' 'wasm-unsafe-eval'`) confines execution to same-origin
// modules plus wasm compilation.
//
// Why a dynamic import and not a `<script type="module" src>`: Trunk's
// wasm-bindgen entry only EXPORTS its init; a bare `<script src>` loads the
// module but never calls init(), so the wasm is never instantiated and the app
// never mounts. We import the module and invoke init() ourselves, passing the
// real hashed `…_bg.wasm` (the entry's built-in default path is the unhashed,
// nonexistent name). Both URLs are the artifacts integrity.js already verified
// and cached, so the import reads exactly those verified bytes.
async function bootTarget(target) {
  show("booting");
  if (ui.bootingVersion) ui.bootingVersion.textContent = target.version;

  // Verify entry JS + wasm + chunks before anything executes. Verified bytes
  // are written into the bundle cache so the <script>/wasm load reads exactly
  // those bytes (and offline relaunch works); a tampered artifact is never
  // cached (#5a) and aborts the boot.
  const cache = await openBundleCache();
  let result;
  try {
    result = await verifyArtifacts(target.artifacts, { cache });
  } catch (err) {
    result = { ok: false, failures: [String(err && err.message ? err.message : err)] };
  }
  if (!result.ok) {
    await deleteCachedArtifacts(target);
    forgetVersion();
    setError(
      "The client bundle failed its integrity check and was not started.",
      `version ${target.version}: ${result.failures.join(", ")}`,
    );
    return;
  }

  // Inject the bundle's own stylesheet(s) before its entry script so the
  // mounted Leptos app is styled (the entry <script> alone carries no CSS). The
  // hrefs come from the version's index.html and are confined to the version
  // path; failure here is non-fatal so a styling hiccup never blocks the boot.
  await injectBundleStyles(target);

  // Resolve the EXPLICIT entry + hashed-wasm URLs from the verified target.
  const urls = selectBootUrls(target);
  if (!urls.ok) {
    await deleteCachedArtifacts(target);
    forgetVersion();
    setError(
      "The client failed to start.",
      `version ${target.version}: ${urls.reason}`,
    );
    return;
  }

  // Boot handoff: the app mounts into the SEPARATE #app-root (see index.html).
  // Set the observer up BEFORE init() runs so a mount that happens synchronously
  // during init() is not missed; `finalizeHandoff` is invoked once init()
  // resolves to hand off immediately if the app already produced DOM.
  const finalizeHandoff = setUpBootHandoff();

  try {
    // Same-origin ES module — permitted by `script-src 'self'`. The SW serves
    // the verified, cached bytes for this `/tyde/v<ver>/…` path.
    const mod = await import(urls.entryUrl);
    const initFn = mod && (mod.default || mod.init);
    if (typeof initFn !== "function") {
      throw new Error("entry module exposes no init()");
    }
    // Explicit hashed wasm path; `'wasm-unsafe-eval'` permits the compilation.
    await initFn({ module_or_path: urls.wasmUrl });
  } catch (err) {
    // Defense in depth: if the boot fails (bad import, wasm instantiate error, a
    // poisoned cache entry slipping past), purge the cached artifacts so a
    // transient tampered 200 can't wedge the user in permanent failure, forget
    // the version, and surface the error. setError re-shows the shell, so a
    // failure after we (somehow) hid it still shows the error view, not a blank
    // page.
    await deleteCachedArtifacts(target);
    forgetVersion();
    setError(
      "The client failed to start.",
      String(err && err.message ? err.message : err),
    );
    return;
  }

  // init() resolved: the app has mounted (or will imminently). Hand off now if
  // it already produced DOM in #app-root; the observer covers a late mount.
  finalizeHandoff();
}

// Hides #loader-shell once the app actually mounts into #app-root. Sets up a
// MutationObserver for the precise moment and returns a `finalize` callback the
// caller invokes once init() resolves. The error path calls neither, so a failed
// boot keeps the shell (and its error view) visible.
function setUpBootHandoff() {
  const appRoot = document.getElementById("app-root");
  if (!appRoot) return () => {};

  let handed = false;
  const handoff = () => {
    if (handed) return;
    handed = true;
    hideLoaderShell();
  };

  if (typeof MutationObserver !== "undefined") {
    const observer = new MutationObserver((mutations) => {
      for (const m of mutations) {
        if (m.addedNodes && m.addedNodes.length > 0) {
          observer.disconnect();
          handoff();
          return;
        }
      }
    });
    observer.observe(appRoot, { childList: true });
  }

  // Called once init() resolves. Common case: the Leptos app mounted
  // synchronously during init(), so #app-root already has children → hand off
  // now. Otherwise give an async mount a few seconds, then hand off only IF it
  // actually produced DOM — so an app that loads but never mounts (the error
  // view should win) does not get the shell yanked out from under it.
  return () => {
    if (appRoot.childNodes.length > 0) {
      handoff();
      return;
    }
    setTimeout(() => {
      if (appRoot.childNodes.length > 0) handoff();
    }, 4000);
  };
}

// Fetches the resolved version's own index.html, extracts its same-origin
// `<link rel="stylesheet">` tags (confined to the version directory, SRI
// preserved), and injects them into the loader document so the booted app has
// its CSS. Best-effort: any failure is swallowed — the app still boots, just
// unstyled, which is strictly better than aborting the boot.
async function injectBundleStyles(target) {
  try {
    const indexPath = target.path + "index.html";
    const origin = typeof location !== "undefined" ? location.origin : null;
    // Absolute base so relative hrefs in the bundle index resolve correctly.
    const baseHref = origin
      ? new URL(indexPath, origin).href
      : "https://loader.invalid" + indexPath;

    const response = await fetch(indexPath, { cache: "no-store" });
    if (!response.ok) return;
    const html = await response.text();

    const sheets = resolveBundleStylesheets(html, {
      baseHref,
      versionPath: target.path,
      origin,
    });

    const existing = new Set(
      Array.from(document.querySelectorAll('link[rel="stylesheet"]')).map((l) =>
        l.getAttribute("href"),
      ),
    );
    for (const sheet of sheets) {
      if (existing.has(sheet.href)) continue;
      const link = document.createElement("link");
      link.rel = "stylesheet";
      link.href = sheet.href;
      // Preserve the bundle's declared SRI + crossorigin verbatim. The
      // stylesheet is same-origin (integrity works without CORS), so we do NOT
      // synthesize a crossOrigin the bundle didn't declare — mirroring exactly
      // how the bundle's own index.html loads it.
      if (sheet.integrity) link.integrity = sheet.integrity;
      if (sheet.crossorigin !== null) link.crossOrigin = sheet.crossorigin;
      document.head.appendChild(link);
      existing.add(sheet.href);
    }
  } catch {
    // Styling is best-effort; never block the boot on it.
  }
}

function rememberVersion(version) {
  try {
    localStorage.setItem(STORAGE_KEY, version);
  } catch {
    // Storage may be unavailable (private mode); returning-user fast path is
    // then simply disabled. Not fatal.
  }
}

function readVersion() {
  try {
    return localStorage.getItem(STORAGE_KEY);
  } catch {
    return null;
  }
}

function forgetVersion() {
  try {
    localStorage.removeItem(STORAGE_KEY);
  } catch {
    /* ignore */
  }
}

function indexedDbFactory() {
  try {
    if (typeof window !== "undefined" && window.indexedDB) return window.indexedDB;
    if (typeof indexedDB !== "undefined") return indexedDB;
  } catch {
    // Accessing indexedDB can throw in restricted/private contexts.
  }
  return null;
}

function idbError(request) {
  const error = request && request.error;
  return error && error.message ? error.message : "indexeddb request failed";
}

function openWebStoreDb(factory) {
  return new Promise((resolve, reject) => {
    let request;
    try {
      request = factory.open(WEB_DB_NAME, WEB_DB_VERSION);
    } catch (err) {
      reject(err);
      return;
    }

    request.onupgradeneeded = () => {
      const db = request.result;
      if (!db.objectStoreNames.contains(WEB_HOSTS_STORE)) {
        db.createObjectStore(WEB_HOSTS_STORE);
      }
      if (!db.objectStoreNames.contains(WEB_PSK_STORE)) {
        db.createObjectStore(WEB_PSK_STORE);
      }
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(new Error(idbError(request)));
    request.onblocked = () => reject(new Error("indexeddb open was blocked"));
  });
}

function idbGet(db, store, key) {
  return new Promise((resolve, reject) => {
    let request;
    try {
      const tx = db.transaction(store, "readonly");
      request = tx.objectStore(store).get(key);
    } catch (err) {
      reject(err);
      return;
    }
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(new Error(idbError(request)));
  });
}

export function decodeStoredPairedHostsState(raw) {
  if (raw === undefined || raw === null) return false;
  if (typeof raw !== "string") return null;
  try {
    const records = JSON.parse(raw);
    return Array.isArray(records) ? records.length > 0 : null;
  } catch {
    return null;
  }
}

// Returns true when the web app has stored paired hosts, false when the browser
// store is present but empty, and null when storage cannot be inspected. The
// loader uses this only to ignore a stale remembered bundle version after all
// hosts have been forgotten; unknown preserves the existing remembered-version
// behavior so storage errors do not strand already-paired users.
export async function hasStoredPairedHosts(factory = indexedDbFactory()) {
  if (!factory || typeof factory.open !== "function") return null;
  let db = null;
  try {
    db = await openWebStoreDb(factory);
    const raw = await idbGet(db, WEB_HOSTS_STORE, WEB_HOSTS_KEY);
    return decodeStoredPairedHostsState(raw);
  } catch {
    return null;
  } finally {
    if (db && typeof db.close === "function") db.close();
  }
}

export function resolveStartupTarget(manifest, remembered, pairedHosts) {
  if (remembered && pairedHosts !== false) {
    const resolved = resolveBootTarget(remembered, manifest);
    if (resolved.ok) return { ...resolved, source: "remembered" };
  }

  const latest = resolveLatestBootTarget(manifest);
  if (latest.ok) return { ...latest, source: "latest" };
  return latest;
}

// True when the loader is running as an INSTALLED PWA — an iOS "Add to Home
// Screen" web app (`navigator.standalone === true`) or any browser reporting
// the standalone display-mode. Pure (globals injectable) so it is unit-testable.
//
// Why this matters: on iOS, scanning the host QR with the native Camera opens
// the link in an EPHEMERAL in-app preview (Safari View Controller), not real
// Safari. Auto-pairing there stashes state in a sessionStorage that is destroyed
// the instant the user taps the X — losing the pairing. So when we are NOT in a
// persistent (installed/standalone) context we must not silently auto-pair; we
// show a guard screen and let the user choose. Inside a normal Safari tab the
// guard still offers an explicit "Pair here anyway", so that user is never
// blocked. Already-installed/standalone keeps auto-pairing exactly as before.
export function isStandaloneContext(win, nav) {
  if (!!nav && nav.standalone === true) return true;
  if (
    !!win &&
    typeof win.matchMedia === "function" &&
    win.matchMedia("(display-mode: standalone)").matches
  ) {
    return true;
  }
  return false;
}

// Cross-checks the host QR's protocol version against the resolved manifest
// entry's stamped `protocolVersion` BEFORE booting. Fails closed: an entry with
// no stamped protocol (packaging drift — the host build's bundle was not
// published with metadata) and a true mismatch both abort the boot rather than
// launching a bundle the WASM would only strict-reject deep inside. This keeps
// the strict equality check intact and adds an explicit, pre-boot failure so the
// user never sees a raw "expected N" from the wrong bundle. Pure; exported for
// unit testing. Returns `{ ok: true }` or `{ ok: false, reason, detail }`.
export function checkProtocolCompatibility(qrProtocolVersion, target) {
  const entryProtocol =
    target && typeof target === "object" ? target.protocolVersion : null;
  if (entryProtocol === null || entryProtocol === undefined) {
    return {
      ok: false,
      reason: "protocol-unpublished",
      detail: `host protocol ${
        Number.isInteger(qrProtocolVersion) ? qrProtocolVersion : "?"
      }, published bundle has no protocol metadata`,
    };
  }
  if (!Number.isInteger(qrProtocolVersion)) {
    return {
      ok: false,
      reason: "protocol-mismatch",
      detail: `pairing code carried no usable protocol version (bundle protocol ${entryProtocol})`,
    };
  }
  if (qrProtocolVersion !== entryProtocol) {
    return {
      ok: false,
      reason: "protocol-mismatch",
      detail: `host protocol ${qrProtocolVersion}, published bundle protocol ${entryProtocol}`,
    };
  }
  return { ok: true };
}

// Reads and CLEARS the self-heal repair URI a stale bundle stashed before it
// asked the loader to reload (see REPAIR_URI_KEY / onRepairNeeded). Returns the
// raw URI string or null. Always clears so a stale URI cannot replay.
export function takeRepairUri() {
  try {
    const uri = sessionStorage.getItem(REPAIR_URI_KEY);
    if (uri !== null && uri !== undefined) sessionStorage.removeItem(REPAIR_URI_KEY);
    return uri && uri.length > 0 ? uri : null;
  } catch {
    return null;
  }
}

// Reads and CLEARS the reconnect self-heal repair version a stale bundle stashed
// before it asked the loader to reload (see REPAIR_VERSION_KEY / onRepairVersion).
// Returns the raw version string or null. Always clears so a stale version cannot
// replay across a later clean load.
export function takeRepairVersion() {
  try {
    const version = sessionStorage.getItem(REPAIR_VERSION_KEY);
    if (version !== null && version !== undefined) {
      sessionStorage.removeItem(REPAIR_VERSION_KEY);
    }
    return version && version.length > 0 ? version : null;
  } catch {
    return null;
  }
}

// Increments and returns the session-scoped self-heal reboot counter. `init()`
// calls this once per repair reboot it processes and breaks the loop when the
// count exceeds MAX_REPAIR_ATTEMPTS. If sessionStorage is unavailable the repair
// stash in `onRepairNeeded` also fails (so no reload loop is possible); we then
// report 1 so the single in-memory attempt still proceeds.
export function registerRepairAttempt() {
  try {
    const raw = sessionStorage.getItem(REPAIR_ATTEMPTS_KEY);
    const current = Number.parseInt(raw || "0", 10);
    const next = (Number.isInteger(current) && current >= 0 ? current : 0) + 1;
    sessionStorage.setItem(REPAIR_ATTEMPTS_KEY, String(next));
    return next;
  } catch {
    return 1;
  }
}

// Resets the self-heal reboot counter — called on a clean (non-repair) load and
// when the loop guard trips, so a later genuine drift can self-heal again.
export function clearRepairAttempts() {
  try {
    sessionStorage.removeItem(REPAIR_ATTEMPTS_KEY);
  } catch {
    /* ignore */
  }
}

// Handles a `tyde:repair-needed` event. The booted mobile bundle dispatches it
// when it detects, during in-app pairing, that its OWN compiled protocol no
// longer matches the host's QR (see mobile-frontend `request_loader_repair`).
// With the carried pairing URI: forget the stale remembered version, stash the
// URI, and reload so the stale WASM is torn down and the fresh loader re-routes
// the URI through the version-matched boot path. Without a URI: just forget and
// return to the pair screen. Returns true when it routed to a repair-reload.
//
// NOTE: only a bundle that CONTAINS this dispatch code self-heals — a bundle
// built before the dispatch was added (e.g. an already-running older beta)
// cannot retroactively gain it, so it still surfaces its own raw error. The
// loop guard in `init()` (REPAIR_ATTEMPTS_KEY) bounds repeated reboots.
// Exported for unit testing; `reload` is injectable so tests don't navigate.
export function onRepairNeeded(detail, reload) {
  forgetVersion();
  const uri = typeof detail === "string" && detail.length > 0 ? detail : null;
  if (!uri) {
    show("pair");
    return false;
  }
  try {
    sessionStorage.setItem(REPAIR_URI_KEY, uri);
  } catch {
    // No sessionStorage (private mode): can't hand off across reload, so fall
    // back to the pair screen for a manual re-scan rather than silently nothing.
    show("pair");
    return false;
  }
  const doReload =
    typeof reload === "function"
      ? reload
      : typeof window !== "undefined" &&
          window.location &&
          typeof window.location.reload === "function"
        ? () => window.location.reload()
        : null;
  if (doReload) doReload();
  return true;
}

// Handles a `tyde:repair-version` event. A booted bundle dispatches it when an
// ALREADY-PAIRED host it reconnected to answered the Tyde handshake with a
// protocol-incompatible reject carrying the host's release version (see
// mobile-frontend `request_loader_repair_version`). Stash the version, forget
// the stale remembered bundle, and reload so the fresh loader boots the
// version-matched bundle for the still-stored paired host — no re-scan. Without
// a usable version: just forget so the next clean load picks a fresh target.
// Returns true when it routed to a repair-reload. The version is only VALIDATED
// (against the manifest) on the reload side, so a bad value fails closed with an
// explicit error rather than silently falling back. Exported for unit testing;
// `reload` is injectable so tests don't navigate.
export function onRepairVersion(detail, reload) {
  forgetVersion();
  const version = typeof detail === "string" && detail.length > 0 ? detail : null;
  if (!version) {
    return false;
  }
  try {
    sessionStorage.setItem(REPAIR_VERSION_KEY, version);
  } catch {
    // No sessionStorage (private mode): can't hand off across a reload, so leave
    // the booted bundle's sticky "update required" error as the surface.
    return false;
  }
  const doReload =
    typeof reload === "function"
      ? reload
      : typeof window !== "undefined" &&
          window.location &&
          typeof window.location.reload === "function"
        ? () => window.location.reload()
        : null;
  if (doReload) doReload();
  return true;
}

// Stash the raw pairing URI for the booted app to consume (see PAIR_URI_KEY).
function stashPairingUri(uri) {
  try {
    sessionStorage.setItem(PAIR_URI_KEY, uri);
  } catch {
    // Without sessionStorage the app simply shows its own pairing screen and
    // the user pastes/scans again — degraded but not broken.
  }
}

// Persist the in-flight pairing fragment so a retry after a pre-commit failure
// (e.g. a failed manifest fetch whose retry reloads the page) can recover it
// without a rescan. PSK stays in sessionStorage, never the URL.
function stashPendingFragment(fragment) {
  try {
    sessionStorage.setItem(PENDING_FRAGMENT_KEY, fragment);
  } catch {
    // No sessionStorage: retry after a failure will need a rescan — degraded,
    // not broken, and never a silent wrong-host pairing.
  }
}

function readPendingFragment() {
  try {
    const value = sessionStorage.getItem(PENDING_FRAGMENT_KEY);
    return value && value.length > 0 ? value : null;
  } catch {
    return null;
  }
}

function clearPendingFragment() {
  try {
    sessionStorage.removeItem(PENDING_FRAGMENT_KEY);
  } catch {
    /* ignore */
  }
}

// Resolves the pairing fragment for this init from the URL hash the caller has
// ALREADY captured and cleared (no-leak invariant). A `tyde-pair://…` fragment
// in the URL is the source of truth: it is returned AND mirrored to
// sessionStorage so a retry after a pre-commit failure can recover it. When the
// URL has no pairing fragment (e.g. a retry reload after we cleared it), we
// recover the previously-stashed one. Returns the fragment string or null.
// Exported for unit testing; only reads/writes sessionStorage (no DOM).
export function resolvePairingFragment(urlHash) {
  let fragment = null;
  if (typeof urlHash === "string" && urlHash.length > 0) {
    const f = urlHash.startsWith("#") ? urlHash.slice(1) : urlHash;
    if (f.includes("tyde-pair://")) fragment = f;
  }
  if (fragment) {
    stashPendingFragment(fragment);
    return fragment;
  }
  return readPendingFragment();
}

// Handles a raw pairing URI (from scan or paste): parse -> validate -> resolve
// against the manifest -> stash URI for the app -> boot. Surfaces a friendly
// error otherwise. The loader trusts ONLY release_version for its own decision;
// the app re-parses the stashed URI authoritatively.
export async function handlePairingUri(uri, manifest) {
  // We are committing to processing a specific URI now, so the in-flight
  // pending-fragment recovery copy has served its purpose — consume it. (A
  // manifest-FETCH failure happens before we ever reach here, so the pending
  // copy survives that and its retry can still recover it; a failure FROM here
  // is a property of the URI itself, so a retry returns to the pair screen for a
  // fresh scan rather than replaying the same explicit error forever.)
  clearPendingFragment();
  // The QR is now a generic HTTPS link whose fragment carries the
  // `tyde-pair://…` URI; normalize to that inner URI so everything downstream
  // (parse + the stashed value the WASM app's `take_pending_pairing_uri`
  // reads) keeps working with a plain `tyde-pair://…` string.
  const inner = extractPairingUri(uri) ?? (typeof uri === "string" ? uri.trim() : uri);
  let parsed;
  try {
    parsed = parsePairingUri(inner);
  } catch (err) {
    setError(
      "That does not look like a Tyde pairing code.",
      String(err && err.message ? err.message : err),
    );
    return;
  }
  if (!parsed.releaseVersion) {
    setError(
      "The pairing code is from a host that is too old to use the web client. Update the host and re-pair.",
      "missing or invalid release_version",
    );
    return;
  }
  const resolved = resolveBootTarget(parsed.releaseVersion, manifest);
  if (!resolved.ok) {
    setError(reasonToMessage(resolved.reason), `version ${parsed.releaseVersion}`);
    return;
  }
  // Fail closed on protocol drift BEFORE booting: if the host's QR protocol does
  // not match the published bundle's stamped protocol (or the bundle has none),
  // surface an explicit packaging-drift error instead of booting a bundle the
  // WASM would only reject deep inside. No stash, no remember, no boot — and no
  // fallback to "latest".
  const protocolCheck = checkProtocolCompatibility(parsed.protocolVersion, resolved);
  if (!protocolCheck.ok) {
    setError(reasonToMessage(protocolCheck.reason), protocolCheck.detail);
    return;
  }
  // Hand the normalized `tyde-pair://…` URI to the booted app so first-time
  // pairing can complete, then remember the version for future no-QR launches.
  stashPairingUri(inner.trim());
  rememberVersion(resolved.version);
  await bootTarget(resolved);
}

// --- QR scanning -----------------------------------------------------------
//
// Scanning needs only a camera (`getUserMedia`). To DECODE a frame we use the
// native BarcodeDetector where available (the fast path on Chrome/Android), and
// otherwise fall back to the bundled pure-JS jsQR decoder (web/loader/vendor/
// jsqr.js, loaded as a same-origin classic <script> that assigns `window.jsQR`).
// jsQR makes scanning work on iOS Safari, which has no BarcodeDetector. Both
// paths funnel a decoded value through the same `handlePairingUri`.

let scanStream = null;
let scanRaf = 0;
let scanCanvas = null;

// Returns true when a decoded QR string is a Tyde pairing code — either the raw
// `tyde-pair://…` form or the generic HTTPS QR that carries the URI in its
// fragment (`…/#tyde-pair://…`). Pure; unit-tested.
export function isPairingScanValue(value) {
  return (
    typeof value === "string" &&
    (value.startsWith("tyde-pair://") || value.includes("#tyde-pair://"))
  );
}

// Runs the bundled jsQR decoder over an ImageData and returns the decoded QR
// string, or null when no QR is found. Pure (no DOM/camera) so the fallback
// decode path is unit-testable with a fixture ImageData. `jsqr` is normally the
// global the vendor <script> installs; it is injectable for tests.
export function decodeQrImageData(imageData, jsqr) {
  const decoder =
    jsqr || (typeof globalThis !== "undefined" ? globalThis.jsQR : undefined);
  if (typeof decoder !== "function" || !imageData) return null;
  let result;
  try {
    result = decoder(imageData.data, imageData.width, imageData.height, {
      inversionAttempts: "attemptBoth",
    });
  } catch {
    return null;
  }
  return result && typeof result.data === "string" ? result.data : null;
}

function stopScan() {
  if (scanRaf) cancelAnimationFrame(scanRaf);
  scanRaf = 0;
  if (scanStream) {
    for (const track of scanStream.getTracks()) track.stop();
    scanStream = null;
  }
  if (ui.video) {
    ui.video.hidden = true;
    ui.video.srcObject = null;
  }
}

// Grabs the current video frame as ImageData via an offscreen <canvas> so jsQR
// can read its pixels. Returns null until the video has real dimensions.
function grabVideoFrame() {
  const video = ui.video;
  if (!video || !video.videoWidth || !video.videoHeight) return null;
  if (!scanCanvas) scanCanvas = document.createElement("canvas");
  scanCanvas.width = video.videoWidth;
  scanCanvas.height = video.videoHeight;
  const ctx = scanCanvas.getContext("2d", { willReadFrequently: true });
  if (!ctx) return null;
  ctx.drawImage(video, 0, 0, scanCanvas.width, scanCanvas.height);
  return ctx.getImageData(0, 0, scanCanvas.width, scanCanvas.height);
}

async function startScan(manifest) {
  const hasCamera =
    navigator.mediaDevices && typeof navigator.mediaDevices.getUserMedia === "function";
  if (!hasCamera) {
    if (ui.scanStatus) {
      ui.scanStatus.textContent =
        "Camera scanning is not available on this browser — paste the pairing code below instead.";
    }
    return;
  }

  // Decode strategy: native BarcodeDetector if present, else the jsQR fallback.
  const hasDetector = "BarcodeDetector" in window;

  try {
    scanStream = await navigator.mediaDevices.getUserMedia({
      video: { facingMode: "environment" },
    });
    ui.video.srcObject = scanStream;
    ui.video.hidden = false;
    await ui.video.play();

    let detector = null;
    if (hasDetector) {
      try {
        // eslint-disable-next-line no-undef
        detector = new BarcodeDetector({ formats: ["qr_code"] });
      } catch {
        detector = null; // unsupported format set, etc. → use jsQR.
      }
    }

    const onDecoded = async (value) => {
      stopScan();
      await handlePairingUri(extractPairingUri(value) ?? value, manifest);
    };

    const tick = async () => {
      if (!scanStream) return;
      try {
        if (detector) {
          const codes = await detector.detect(ui.video);
          const hit = codes.find((c) => isPairingScanValue(c.rawValue));
          if (hit) {
            await onDecoded(hit.rawValue);
            return;
          }
        } else {
          const frame = grabVideoFrame();
          const value = frame ? decodeQrImageData(frame) : null;
          if (isPairingScanValue(value)) {
            await onDecoded(value);
            return;
          }
        }
      } catch {
        // Transient detect/draw failures are ignored; keep scanning.
      }
      scanRaf = requestAnimationFrame(tick);
    };
    scanRaf = requestAnimationFrame(tick);
  } catch (err) {
    if (ui.scanStatus) {
      ui.scanStatus.textContent =
        "Could not access the camera — paste the pairing code below instead.";
    }
    stopScan();
    void err;
  }
}

// --- Safari-guard for in-app-browser pairing links -------------------------
//
// Shows #view-pair-guard for a pairing link opened in a non-persistent context.
// The pairing URI lives in this closure and is mirrored in sessionStorage under
// PENDING_FRAGMENT_KEY (so a retry can recover it without a rescan), but it is
// NEVER re-written to the URL/history/referrer — init() cleared the fragment
// before any await, and sessionStorage is not sent to the origin. So dismissing
// the in-app sheet doesn't leak it and "Pair here anyway" still has it.
function showPairGuard(pairingUri, manifest) {
  showLoaderShell();

  // Best-effort "Open in Safari": re-attach the pairing URI as a fragment on a
  // plain https link to this same loader. iOS has no reliable programmatic
  // "escape to Safari", so this is a tap target the user can long-press →
  // "Open in Safari", or that may hand off depending on OS/version. The secret
  // rides in the fragment (never sent to the origin), preserving the no-leak
  // property; it only ever lives in this anchor's href, not the current URL.
  if (ui.guardOpenSafari) {
    const base =
      typeof location !== "undefined"
        ? location.origin + location.pathname
        : "";
    ui.guardOpenSafari.href = base + "#" + pairingUri;
  }

  if (ui.guardPairAnyway && !ui.guardPairAnyway.__wired) {
    ui.guardPairAnyway.__wired = true;
    ui.guardPairAnyway.addEventListener("click", () => {
      void handlePairingUri(pairingUri, manifest);
    });
  }

  show("pairGuard");
}

// Routes a `tyde-pair://…`-bearing URL fragment detected on boot. In an
// installed/standalone PWA the context is persistent, so we auto-pair exactly
// as before. Otherwise (e.g. iOS's ephemeral in-app Camera preview / Safari
// View Controller) we do NOT silently auto-pair — the pairing would be lost
// when the preview is dismissed — and instead show the guard, keeping the URI
// in memory so "Pair here anyway" can still proceed. The caller MUST have
// already cleared the URL fragment before invoking this (no-leak invariant).
// `standalone` is injectable for tests; it defaults to live detection.
export async function routePairingFragment(fragment, manifest, standalone) {
  const pairingUri = extractPairingUri(fragment) ?? fragment;
  const persistent =
    standalone !== undefined
      ? standalone
      : isStandaloneContext(
          typeof window !== "undefined" ? window : undefined,
          typeof navigator !== "undefined" ? navigator : undefined,
        );
  if (persistent) {
    await handlePairingUri(pairingUri, manifest);
  } else {
    showPairGuard(pairingUri, manifest);
  }
}

// --- Service worker --------------------------------------------------------

function registerServiceWorker() {
  if (!("serviceWorker" in navigator)) return;
  // Registered relative to the loader scope so it controls `/tyde/`.
  navigator.serviceWorker.register("./sw.js", { scope: "./" }).catch(() => {
    // Offline support is a progressive enhancement; failure is non-fatal.
  });
}

// Applies a `pairingUiState` to the pair screen: lead with scanning where the
// browser supports it, otherwise hide the scan button and make the paste flow
// the obvious primary action. Returns the state so callers can decide whether
// to wire up the scan button at all.
function applyPairingUi(state) {
  if (ui.scanButton) {
    ui.scanButton.hidden = state.scanButtonHidden;
    ui.scanButton.classList.toggle("primary", state.scanButtonPrimary);
    ui.scanButton.classList.toggle("secondary", !state.scanButtonPrimary);
  }
  if (ui.connectButton) {
    ui.connectButton.classList.toggle("primary", state.pastePrimary);
    ui.connectButton.classList.toggle("secondary", !state.pastePrimary);
  }
  if (ui.pairStatus) ui.pairStatus.textContent = state.instruction;
  if (ui.pasteLabel) ui.pasteLabel.textContent = state.pasteLabel;
  return state;
}

// --- Entry point -----------------------------------------------------------

async function init() {
  ui.loading = $("view-loading");
  ui.pair = $("view-pair");
  ui.pairGuard = $("view-pair-guard");
  ui.guardOpenSafari = $("guard-open-safari");
  ui.guardPairAnyway = $("guard-pair-anyway");
  ui.booting = $("view-booting");
  ui.error = $("view-error");
  ui.errorMessage = $("error-message");
  ui.errorDetail = $("error-detail");
  ui.bootingVersion = $("booting-version");
  ui.video = $("scan-video");
  ui.scanStatus = $("scan-status");
  ui.pasteForm = $("paste-form");
  ui.pasteInput = $("paste-input");
  ui.pasteLabel = $("paste-label");
  ui.connectButton = $("connect-button");
  ui.scanButton = $("scan-button");
  ui.pairStatus = $("pair-status");
  ui.retryButton = $("retry-button");

  // Bind the retry button FIRST, before any early return below, so a
  // manifest-fetch or returning-user boot error still leaves a working "Try
  // again" button (#8).
  if (ui.retryButton) {
    ui.retryButton.addEventListener("click", () => window.location.reload());
  }

  // Debug/escape hatch for support: clear a wedged stored version from the
  // console without DevTools storage spelunking.
  window.__tydeLoader = { forgetVersion, version: () => readVersion() };

  registerServiceWorker();

  // The booted mobile bundle dispatches this when an IN-APP pairing attempt hits
  // a protocol mismatch against the host's QR (mobile-frontend
  // `request_loader_repair`). When it carries the host's pairing URI
  // (`event.detail`), self-heal by rebooting into the version-matched bundle;
  // otherwise return to pairing. (Only bundles that ship this dispatch can
  // self-heal — an older bundle without it just shows its own error.)
  window.addEventListener("tyde:repair-needed", (event) => {
    const detail =
      event && typeof event.detail === "string" ? event.detail : null;
    onRepairNeeded(detail);
  });

  // Reconnect self-heal: the booted bundle reconnected to an already-paired host
  // whose upgraded build now rejects this bundle's protocol, and forwarded the
  // host's release version (mobile-frontend `request_loader_repair_version`).
  // Reboot into the version-matched bundle for the still-stored paired host —
  // no re-scan. Bounded by the same REPAIR_ATTEMPTS_KEY loop guard as the URI
  // path (handled in the repair block below).
  window.addEventListener("tyde:repair-version", (event) => {
    const detail =
      event && typeof event.detail === "string" ? event.detail : null;
    onRepairVersion(detail);
  });

  // Capture + IMMEDIATELY clear any URL fragment BEFORE the first await or early
  // return below, so the PSK-bearing `tyde-pair://…` fragment can never leak
  // into a later navigation/referrer regardless of which path init() takes
  // (manifest fetch, self-heal repair, startup boot). The host's QR is a generic
  // HTTPS link (`https://tycode.dev/tyde/#tyde-pair://v1?<payload>`); the secret
  // rides in the FRAGMENT, which browsers never send to the origin.
  // `resolvePairingFragment` mirrors the captured fragment into sessionStorage
  // (never the URL) so a retry after a pre-commit failure — e.g. a failed
  // manifest fetch, whose retry button reloads the page — can recover it without
  // a rescan, and recovers a previously-stashed one when this load has no hash.
  // `history.replaceState` runs only when a hash was present, preserving the
  // no-rewrite-on-clean-load behavior.
  const hash = window.location.hash;
  if (hash) {
    history.replaceState(null, "", window.location.pathname + window.location.search);
  }
  const pairingFragment = resolvePairingFragment(hash);

  show("loading");

  let manifest;
  try {
    manifest = await fetchManifest();
  } catch (err) {
    setError(reasonToMessage("no-manifest"), String(err && err.message ? err.message : err));
    return;
  }

  // Self-heal re-pair: a booted bundle dispatched `tyde:repair-needed` with the
  // host's pairing URI on an in-app protocol mismatch, which we stashed before
  // reloading. Re-route it through the version-matched boot path so the MATCHING
  // bundle authoritatively pairs (and if none is published, the user sees an
  // explicit failure — never a silent downgrade). Runs before the fragment/
  // startup fast paths.
  //
  // LOOP BREAKER: a corrupt manifest could stamp a protocol matching the QR yet
  // serve a bundle whose compiled protocol differs, so the booted bundle
  // re-dispatches repair every reload. Cap the reboots: after MAX_REPAIR_ATTEMPTS
  // clear repair state and show an explicit error instead of reloading again.
  const repairUri = takeRepairUri();
  // A version-only repair (reconnect self-heal) is mutually exclusive with a
  // URI repair (in-app pairing self-heal); prefer the URI path when both were
  // stashed and consume the version stash either way so it can't replay later.
  const repairVersion = takeRepairVersion();
  if (repairUri || repairVersion) {
    const attempts = registerRepairAttempt();
    if (attempts > MAX_REPAIR_ATTEMPTS) {
      clearRepairAttempts();
      setError(
        reasonToMessage("repair-loop"),
        `gave up after ${MAX_REPAIR_ATTEMPTS} self-heal attempt(s)`,
      );
      return;
    }
    if (repairUri) {
      await handlePairingUri(repairUri, manifest);
      return;
    }
    // Version-only self-heal: the paired host is already stored, so resolve its
    // published bundle by release version and boot it directly. Strict and
    // fail-closed — a blocked/unpublished/malformed version surfaces an explicit
    // error, never a silent fallback to the latest bundle. If the manifest is
    // drifted (stamps a matching protocol but serves a mismatched bundle) the
    // rebooted bundle re-rejects and re-dispatches, and the loop guard above
    // stops it after MAX_REPAIR_ATTEMPTS.
    const resolved = resolveBootTarget(repairVersion, manifest);
    if (!resolved.ok) {
      setError(reasonToMessage(resolved.reason), `version ${repairVersion}`);
      return;
    }
    rememberVersion(resolved.version);
    await bootTarget(resolved);
    return;
  }
  // Clean (non-repair) entry: reset the loop guard so a later genuine drift can
  // self-heal again.
  clearRepairAttempts();

  // Auto-pair from the URL fragment captured/cleared above.
  if (pairingFragment) {
    await routePairingFragment(pairingFragment, manifest);
    return;
  }

  // Startup fast path. If there are paired hosts, prefer the remembered
  // host-specific bundle version so an older paired host can still boot. If
  // there are NO paired hosts, ignore that stale remembered version and boot
  // the newest published client so first-time onboarding/scanning uses the
  // latest bug fixes. If IndexedDB cannot be inspected, preserve the old
  // remembered-version behavior for already-paired users.
  const remembered = readVersion();
  const pairedHosts = await hasStoredPairedHosts();
  const startup = resolveStartupTarget(manifest, remembered, pairedHosts);
  if (startup.ok) {
    if (remembered && startup.source !== "remembered") forgetVersion();
    await bootTarget(startup);
    return;
  }
  if (remembered) forgetVersion();

  // Pairing flow. Decide the layout from real browser capability FIRST so the
  // pair screen never leads with a scan button on a browser that can't scan
  // (e.g. Safari/iOS), then reveal it.
  const capability = detectScanCapability(
    typeof window !== "undefined" ? window : undefined,
    typeof navigator !== "undefined" ? navigator : undefined,
  );
  const state = applyPairingUi(pairingUiState(capability));
  show("pair");
  if (ui.pasteForm) {
    ui.pasteForm.addEventListener("submit", (event) => {
      event.preventDefault();
      const value = ui.pasteInput ? ui.pasteInput.value : "";
      void handlePairingUri(value, manifest);
    });
  }
  // Only wire scanning where it actually works; otherwise the button stays
  // hidden and the paste flow carries the user.
  if (ui.scanButton && state.scanAvailable) {
    ui.scanButton.addEventListener("click", () => void startScan(manifest));
  }
}

if (typeof document !== "undefined") {
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", () => void init());
  } else {
    void init();
  }
}
