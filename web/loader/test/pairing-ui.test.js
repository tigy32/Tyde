// Tests for the capability-driven pairing-screen policy (pairing-ui.js).
//
// Scanning now requires only a camera (`getUserMedia`): the loader bundles the
// pure-JS jsQR decoder, so a browser without the native `BarcodeDetector`
// (notably iOS Safari) can still scan via the JS fallback. These tests pin that
// scanning is offered whenever a camera exists, and that paste becomes the
// primary flow only when there is no camera at all.

import { test } from "node:test";
import assert from "node:assert/strict";

import { detectScanCapability, pairingUiState } from "../pairing-ui.js";

const withCamera = { mediaDevices: { getUserMedia: () => {} } };

test("detectScanCapability offers scanning whenever a camera exists", () => {
  // Native detector + camera: scan available, detector reported true.
  assert.deepEqual(
    detectScanCapability({ BarcodeDetector: function () {} }, withCamera),
    { hasDetector: true, hasCamera: true, scanAvailable: true },
  );
  // Safari/iOS: camera present, NO BarcodeDetector — scanning is still
  // available because the loader ships the jsQR fallback.
  assert.deepEqual(detectScanCapability({}, withCamera), {
    hasDetector: false,
    hasCamera: true,
    scanAvailable: true,
  });
  // Detector present, but no camera API → cannot scan (nothing to decode).
  assert.equal(
    detectScanCapability({ BarcodeDetector: function () {} }, {}).scanAvailable,
    false,
  );
  // No globals at all (Node) → unavailable, no throw.
  assert.equal(detectScanCapability(undefined, undefined).scanAvailable, false);
});

test("pairingUiState leads with scanning when available", () => {
  const s = pairingUiState({ scanAvailable: true });
  assert.equal(s.scanAvailable, true);
  assert.equal(s.scanButtonHidden, false);
  assert.equal(s.scanButtonPrimary, true);
  assert.equal(s.pastePrimary, false);
  assert.match(s.pasteLabel, /Or paste/i);
});

test("pairingUiState hides scanning and makes paste primary when unavailable", () => {
  const s = pairingUiState({ scanAvailable: false });
  assert.equal(s.scanAvailable, false);
  assert.equal(s.scanButtonHidden, true);
  assert.equal(s.scanButtonPrimary, false);
  assert.equal(s.pastePrimary, true);
  // The paste instruction must clearly tell the user to paste.
  assert.match(s.pasteLabel, /paste the pairing code/i);
});

test("pairingUiState treats missing/garbage capability as scan-unavailable", () => {
  for (const bad of [undefined, null, {}, { scanAvailable: "yes" }]) {
    assert.equal(pairingUiState(bad).pastePrimary, true);
  }
});
