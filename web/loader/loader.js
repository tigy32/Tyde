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
import { resolveBootTarget } from "./manifest-policy.js";
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
const BUNDLE_CACHE = "tyde-bundle-v1"; // shared with sw.js

const ui = {};

function $(id) {
  return document.getElementById(id);
}

function show(view) {
  for (const name of ["loading", "pair", "booting", "error"]) {
    const el = ui[name];
    if (el) el.hidden = name !== view;
  }
}

function setError(message, detail) {
  show("error");
  if (ui.errorMessage) ui.errorMessage.textContent = message;
  if (ui.errorDetail) ui.errorDetail.textContent = detail || "";
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
    case "invalid-version":
      return "The pairing code carried an invalid version. Scan a fresh QR code.";
    case "bad-policy":
    case "bad-entry-path":
    case "bad-integrity":
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

// Boots a resolved target: SRI-verify every executable artifact, then inject
// the entry module. The URLs and integrity hashes come entirely from the
// manifest — never from the QR text — and the page CSP
// (`script-src 'self' 'wasm-unsafe-eval'`) confines execution to same-origin
// scripts plus wasm compilation.
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

  const script = document.createElement("script");
  script.type = "module";
  script.src = target.entry; // manifest-controlled, validated `/tyde/...` path
  script.integrity = target.integrity; // manifest-controlled SRI digest
  script.crossOrigin = "anonymous";
  script.addEventListener("error", () => {
    // Defense in depth: if the entry still fails to load (e.g. a poisoned cache
    // entry slips past), purge the cached artifacts so a transient tampered 200
    // can't wedge the user in permanent SRI-failure, and fall back to pairing.
    void deleteCachedArtifacts(target);
    forgetVersion();
    setError(
      "The client bundle failed its integrity check or could not load.",
      `version ${target.version}`,
    );
  });
  document.head.appendChild(script);
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

// Stash the raw pairing URI for the booted app to consume (see PAIR_URI_KEY).
function stashPairingUri(uri) {
  try {
    sessionStorage.setItem(PAIR_URI_KEY, uri);
  } catch {
    // Without sessionStorage the app simply shows its own pairing screen and
    // the user pastes/scans again — degraded but not broken.
  }
}

// Handles a raw pairing URI (from scan or paste): parse -> validate -> resolve
// against the manifest -> stash URI for the app -> boot. Surfaces a friendly
// error otherwise. The loader trusts ONLY release_version for its own decision;
// the app re-parses the stashed URI authoritatively.
export async function handlePairingUri(uri, manifest) {
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
  // Hand the normalized `tyde-pair://…` URI to the booted app so first-time
  // pairing can complete, then remember the version for future no-QR launches.
  stashPairingUri(inner.trim());
  rememberVersion(resolved.version);
  await bootTarget(resolved);
}

// --- QR scanning -----------------------------------------------------------
//
// Uses the native BarcodeDetector where available (Chrome/Android, recent
// Safari). Everywhere else we feature-detect and steer the user to the paste
// fallback, which always works.

let scanStream = null;
let scanRaf = 0;

function stopScan() {
  if (scanRaf) cancelAnimationFrame(scanRaf);
  scanRaf = 0;
  if (scanStream) {
    for (const track of scanStream.getTracks()) track.stop();
    scanStream = null;
  }
  if (ui.video) ui.video.hidden = true;
}

async function startScan(manifest) {
  const hasDetector = "BarcodeDetector" in window;
  const hasCamera =
    navigator.mediaDevices && typeof navigator.mediaDevices.getUserMedia === "function";
  if (!hasDetector || !hasCamera) {
    if (ui.scanStatus) {
      ui.scanStatus.textContent =
        "Camera scanning is not available on this browser — paste the pairing code below instead.";
    }
    return;
  }
  try {
    // eslint-disable-next-line no-undef
    const detector = new BarcodeDetector({ formats: ["qr_code"] });
    scanStream = await navigator.mediaDevices.getUserMedia({
      video: { facingMode: "environment" },
    });
    ui.video.srcObject = scanStream;
    ui.video.hidden = false;
    await ui.video.play();

    const tick = async () => {
      if (!scanStream) return;
      try {
        const codes = await detector.detect(ui.video);
        // Match both the legacy raw `tyde-pair://…` QR and the generic HTTPS QR
        // that carries the URI in its fragment (`…/#tyde-pair://…`).
        const hit = codes.find(
          (c) =>
            c.rawValue &&
            (c.rawValue.startsWith("tyde-pair://") || c.rawValue.includes("#tyde-pair://")),
        );
        if (hit) {
          stopScan();
          await handlePairingUri(extractPairingUri(hit.rawValue) ?? hit.rawValue, manifest);
          return;
        }
      } catch {
        // Transient detect failures are ignored; keep scanning.
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

  // A backgrounded host that upgrades will reject the old client at handshake;
  // the WASM app then dispatches this event so the loader forgets the stale
  // version and returns to pairing (self-healing re-pair flow).
  window.addEventListener("tyde:repair-needed", () => {
    forgetVersion();
    show("pair");
  });

  show("loading");

  let manifest;
  try {
    manifest = await fetchManifest();
  } catch (err) {
    setError(reasonToMessage("no-manifest"), String(err && err.message ? err.message : err));
    return;
  }

  // Auto-pair from the URL fragment. The host's QR is a generic HTTPS link
  // (`https://tycode.dev/tyde/#tyde-pair://v1?<payload>`) so the native iOS/
  // Android Camera can open it.
  //
  // SECURITY: the PSK-bearing `tyde-pair://…` URI rides in the FRAGMENT (after
  // `#`), which browsers never send to the origin — so the secret never
  // reaches S3/CloudFront. We clear the fragment IMMEDIATELY (before any await
  // that could let it leak into a later navigation/referrer) via
  // `history.replaceState`, then pair from the in-memory copy.
  const hash = window.location.hash;
  if (hash) {
    const fragment = hash.startsWith("#") ? hash.slice(1) : hash;
    history.replaceState(null, "", window.location.pathname + window.location.search);
    if (fragment.includes("tyde-pair://")) {
      await handlePairingUri(fragment, manifest);
      return;
    }
  }

  // Returning-user fast path: boot the remembered version with no QR, as long
  // as it is still allowed by the (freshly fetched) manifest. If it is gone or
  // now blocked, fall through to pairing.
  const remembered = readVersion();
  if (remembered) {
    const resolved = resolveBootTarget(remembered, manifest);
    if (resolved.ok) {
      await bootTarget(resolved);
      return;
    }
    forgetVersion();
  }

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
