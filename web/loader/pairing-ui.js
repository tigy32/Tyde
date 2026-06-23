// Pairing-screen layout policy. Pure module (no DOM) so the capability-driven
// UI decision is unit-testable under `node --test`.
//
// The loader pairing screen offers two ways to enter a pairing code: scan a QR
// with the camera, or paste the code. Camera scanning needs BOTH the native
// `BarcodeDetector` API and `getUserMedia`. Safari/iOS — the most common
// first-touch browser for the web client — has neither, so leading with a "Scan
// QR code" button there just produces a "not available on this browser" dead
// end. This module decides, from the detected capability, whether to lead with
// scanning or with the paste flow, and supplies the matching copy.

// Detects whether live QR scanning is possible from the given `window` /
// `navigator`. Both the detector and a camera are required. Tolerates undefined
// globals (Node) by reporting "unavailable".
export function detectScanCapability(win, nav) {
  const hasDetector = !!win && "BarcodeDetector" in win;
  const hasCamera =
    !!nav &&
    !!nav.mediaDevices &&
    typeof nav.mediaDevices.getUserMedia === "function";
  return { hasDetector, hasCamera, scanAvailable: hasDetector && hasCamera };
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
