// Pairing-screen layout policy. Pure module (no DOM) so the capability-driven
// UI decision is unit-testable under `node --test`.
//
// The loader pairing screen offers two ways to enter a pairing code: scan a QR
// with the camera, or paste the code. Scanning needs a camera (`getUserMedia`);
// the actual decode uses the native `BarcodeDetector` where present (fast path,
// Chrome/Android) and otherwise falls back to the bundled pure-JS jsQR decoder.
// Because jsQR is always shipped with the loader, a camera ALONE is now enough
// to scan — including on iOS Safari, which has no `BarcodeDetector`. Only when
// there is no camera at all do we hide scanning and make paste the primary flow.

// Detects whether live QR scanning is possible from the given `window` /
// `navigator`. A camera (`getUserMedia`) is required; the decoder is either the
// native `BarcodeDetector` or the bundled jsQR fallback, so the detector is no
// longer required for scanning to be available. Tolerates undefined globals
// (Node) by reporting "unavailable".
export function detectScanCapability(win, nav) {
  const hasDetector = !!win && "BarcodeDetector" in win;
  const hasCamera =
    !!nav &&
    !!nav.mediaDevices &&
    typeof nav.mediaDevices.getUserMedia === "function";
  return { hasDetector, hasCamera, scanAvailable: hasCamera };
}

// Maps a capability to the concrete pairing-screen state the loader applies to
// the DOM. When scanning is available the scan button leads (primary) and paste
// is the secondary fallback; otherwise the scan button is hidden entirely and
// the paste flow becomes the obvious primary action with explicit instructions.
export function pairingUiState(capability) {
  const scanAvailable = !!capability && capability.scanAvailable === true;
  if (scanAvailable) {
    return {
      scanAvailable: true,
      scanButtonHidden: false,
      scanButtonPrimary: true,
      pastePrimary: false,
      instruction: "Scan the pairing QR code from your Tyde host to begin.",
      pasteLabel: "Or paste the pairing code",
    };
  }
  return {
    scanAvailable: false,
    scanButtonHidden: true,
    scanButtonPrimary: false,
    pastePrimary: true,
    instruction: "Connect to your Tyde host to begin.",
    pasteLabel: "Paste the pairing code from your Tyde host",
  };
}
