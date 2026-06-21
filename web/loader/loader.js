// Tyde web loader — the thin, stable shell served at the origin root
// (`https://tycode.dev/tyde/`). It learns the paired host's release version
// from the pairing QR, validates it, looks it up in the server-controlled
// manifest (the allowlist authority), and boots the matching immutable
// versioned bundle under a strict CSP with Subresource Integrity.
//
// This file is the ONLY part of the loader that touches the DOM/network. All
// parsing and policy logic lives in the pure, unit-tested modules it imports.
//
// Keep this file small and rarely-changed: it is the one piece that is NOT
// versioned, so every byte here is permanent surface.

import { parsePairingUri } from "./pairing.js";
import { resolveBootTarget } from "./manifest-policy.js";

const MANIFEST_URL = "./manifest.json";
const STORAGE_KEY = "tyde.loader.version"; // last successfully-paired version

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
  // Network-first (the service worker also keeps a fallback copy): the manifest
  // is the security authority, so we want the freshest allowlist we can get.
  const response = await fetch(MANIFEST_URL, { cache: "no-store" });
  if (!response.ok) {
    throw new Error(`manifest fetch failed: HTTP ${response.status}`);
  }
  return response.json();
}

// Boots a resolved target by injecting its entry module with SRI. The URL and
// integrity come entirely from the manifest — never from the QR text — and the
// page's CSP (`script-src 'self'`) confines execution to same-origin scripts.
function bootTarget(target) {
  show("booting");
  if (ui.bootingVersion) ui.bootingVersion.textContent = target.version;

  const script = document.createElement("script");
  script.type = "module";
  script.src = target.entry; // manifest-controlled, validated `/tyde/...` path
  script.integrity = target.integrity; // manifest-controlled SRI digest
  script.crossOrigin = "anonymous";
  script.addEventListener("error", () => {
    // Integrity mismatch or load failure: drop the remembered version so the
    // user is not trapped, and fall back to pairing.
    forgetVersion();
    setError(
      "The client bundle failed its integrity check or could not load.",
      `version ${target.version}`,
    );
  });
  document.head.appendChild(script);
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

// Handles a raw pairing URI (from scan or paste): parse -> validate -> resolve
// against the manifest -> boot. Surfaces a friendly error otherwise.
async function handlePairingUri(uri, manifest) {
  let parsed;
  try {
    parsed = parsePairingUri(uri);
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
  rememberVersion(resolved.version);
  bootTarget(resolved);
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
        const hit = codes.find((c) => c.rawValue && c.rawValue.startsWith("tyde-pair://"));
        if (hit) {
          stopScan();
          await handlePairingUri(hit.rawValue, manifest);
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
  ui.scanButton = $("scan-button");
  ui.retryButton = $("retry-button");

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

  // Returning-user fast path: boot the remembered version with no QR, as long
  // as it is still allowed by the (freshly fetched) manifest. If it is gone or
  // now blocked, fall through to pairing.
  const remembered = readVersion();
  if (remembered) {
    const resolved = resolveBootTarget(remembered, manifest);
    if (resolved.ok) {
      bootTarget(resolved);
      return;
    }
    forgetVersion();
  }

  // Pairing flow.
  show("pair");
  if (ui.pasteForm) {
    ui.pasteForm.addEventListener("submit", (event) => {
      event.preventDefault();
      const value = ui.pasteInput ? ui.pasteInput.value : "";
      void handlePairingUri(value, manifest);
    });
  }
  if (ui.scanButton) {
    ui.scanButton.addEventListener("click", () => void startScan(manifest));
  }
  if (ui.retryButton) {
    ui.retryButton.addEventListener("click", () => window.location.reload());
  }

  // Debug/escape hatch for support: lets a user clear a wedged stored version
  // from the console without DevTools storage spelunking.
  window.__tydeLoader = { forgetVersion, version: () => readVersion() };
}

if (typeof document !== "undefined") {
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", () => void init());
  } else {
    void init();
  }
}
