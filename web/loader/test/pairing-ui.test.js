// Tests for the capability-driven pairing-screen policy (pairing-ui.js). The
// reported bug was Safari/iOS leading with a "Scan QR code" button that just
// errors; these tests pin that scanning is offered ONLY when both the detector
// and a camera exist, and that otherwise the paste flow becomes primary.

import { test } from "node:test";
import assert from "node:assert/strict";

import { detectScanCapability, pairingUiState } from "../pairing-ui.js";

const withCamera = { mediaDevices: { getUserMedia: () => {} } };

test("detectScanCapability requires BOTH BarcodeDetector and getUserMedia", () => {
  assert.deepEqual(
    detectScanCapability({ BarcodeDetector: function () {} }, withCamera),
    { hasDetector: true, hasCamera: true, scanAvailable: true },
  );
  // Safari/iOS: camera present, no BarcodeDetector.
  assert.equal(detectScanCapability({}, withCamera).scanAvailable, false);
  // Detector present, but no camera API.
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
