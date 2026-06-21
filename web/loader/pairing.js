// Parsing + validation for the `tyde-pair://v1?` pairing URI.
//
// This module is intentionally pure (no DOM, no network) so it can be unit
// tested under `node --test` and reused in the browser unchanged.
//
// SECURITY: nothing here trusts the QR content. We extract `release_version`
// only to look it up in the server-controlled manifest; the real WASM app
// re-parses the full payload authoritatively. The version is validated with
// the SAME rules as Rust's `host-config::TydeReleaseVersion`
// (`host-config/src/lib.rs::validate_release_version`) before it is ever used.

import { decodeFirst } from "./cbor.js";

export const PAIRING_PREFIX = "tyde-pair://v1?";

// Decodes a URL-safe base64 (no padding) string to bytes. Rejects any
// character outside the base64url alphabet so malformed QR text fails loudly
// instead of being silently coerced.
export function base64urlToBytes(input) {
  if (typeof input !== "string") {
    throw new Error("base64url payload must be a string");
  }
  if (!/^[A-Za-z0-9_-]*$/.test(input)) {
    throw new Error("payload contains non-base64url characters");
  }
  const padLen = input.length % 4;
  const padded =
    input.replace(/-/g, "+").replace(/_/g, "/") +
    (padLen === 0 ? "" : "=".repeat(4 - padLen));
  const binary = atob(padded);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    out[i] = binary.charCodeAt(i);
  }
  return out;
}

// Mirror of `host-config::validate_release_version`. Returns the normalized
// (trimmed, `v`-stripped) version string when valid, or `null` when not.
//
// Rules (must match Rust exactly):
//   - trim, then strip a single leading `v`
//   - must not be empty
//   - must not contain `/` or `\` (path separators)
//   - must not contain any whitespace
//   - core must be exactly `major.minor.patch`, each part non-empty ASCII digits
//   - optional prerelease (after the first `-`): non-empty, each dot-separated
//     identifier non-empty and limited to ASCII letters, digits, and `-`
export function validateReleaseVersion(raw) {
  if (typeof raw !== "string") return null;
  let value = raw.trim();
  if (value.startsWith("v")) value = value.slice(1);

  if (value.length === 0) return null;
  if (value.includes("/") || value.includes("\\")) return null;
  if (/\s/.test(value)) return null;

  const dash = value.indexOf("-");
  const core = dash === -1 ? value : value.slice(0, dash);
  const prerelease = dash === -1 ? null : value.slice(dash + 1);

  const parts = core.split(".");
  if (parts.length !== 3) return null;
  for (const part of parts) {
    if (part.length === 0 || !/^[0-9]+$/.test(part)) return null;
  }

  if (prerelease !== null) {
    if (prerelease.length === 0) return null;
    for (const id of prerelease.split(".")) {
      if (id.length === 0 || !/^[0-9A-Za-z-]+$/.test(id)) return null;
    }
  }

  return value;
}

// Parses a pairing URI and extracts only the fields the loader needs. Throws on
// structural problems (bad prefix, bad base64, non-map CBOR). The returned
// `releaseVersion` is the validated/normalized string, or `null` if the field
// is absent or fails validation (`releaseVersionRaw` preserves the original for
// diagnostics).
export function parsePairingUri(uri) {
  if (typeof uri !== "string") {
    throw new Error("pairing URI must be a string");
  }
  const trimmed = uri.trim();
  if (!trimmed.startsWith(PAIRING_PREFIX)) {
    throw new Error("not a tyde-pair://v1 pairing URI");
  }
  const encoded = trimmed.slice(PAIRING_PREFIX.length);
  if (encoded.length === 0) {
    throw new Error("pairing payload is empty");
  }

  const bytes = base64urlToBytes(encoded);
  const map = decodeFirst(bytes);
  if (map === null || typeof map !== "object" || Array.isArray(map)) {
    throw new Error("pairing payload is not a CBOR map");
  }

  const v = typeof map.v === "number" ? map.v : null;
  const protocolVersion =
    typeof map.protocol_version === "number" ? map.protocol_version : null;
  const releaseVersionRaw =
    typeof map.release_version === "string" ? map.release_version : null;
  const releaseVersion = validateReleaseVersion(releaseVersionRaw);

  return { v, protocolVersion, releaseVersion, releaseVersionRaw };
}
