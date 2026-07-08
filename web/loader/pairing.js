// Parsing + validation for supported `tyde-pair://` pairing URIs.
//
// This module is intentionally pure (no DOM, no network) so it can be unit
// tested under `node --test` and reused in the browser unchanged.
//
// SECURITY: nothing here trusts the QR content. We extract `release_version`
// only to look it up in the server-controlled manifest; the real WASM app
// re-parses the full payload authoritatively. The version is validated with
// the SAME rules as Rust's `host-config::TydeReleaseVersion`
// (`host-config/src/lib.rs::validate_release_version`) before it is ever used.

import { MAX_CBOR_BYTES } from "./cbor.js";

export const PAIRING_V1_PREFIX = "tyde-pair://v1?";
export const PAIRING_V2_PREFIX = "tyde-pair://v2?";
export const PAIRING_PREFIX = PAIRING_V1_PREFIX;

const SUPPORTED_PAIRING_PREFIXES = [PAIRING_V1_PREFIX, PAIRING_V2_PREFIX];

// Pulls the inner `tyde-pair://…` pairing URI out of whatever was scanned or
// pasted. The host's QR is now a generic HTTPS link
// (`https://tycode.dev/tyde/#tyde-pair://v2?<payload>`) so the native iOS/
// Android Camera can open it; the PSK-bearing URI rides in the URL FRAGMENT
// (after `#`) and so is never sent to the origin. This accepts either form:
//   - a raw `tyde-pair://…` string (legacy QR / in-app scanner / paste), or
//   - an HTTPS URL whose fragment carries the `tyde-pair://…` URI.
// Returns the inner `tyde-pair://…` string, or null when none is present. The
// fragment is matched as-is first, then a guarded `decodeURIComponent` retry
// covers cameras/links that percent-encode the fragment.
export function extractPairingUri(raw) {
  if (typeof raw !== "string") return null;
  if (raw.length > MAX_URI_LEN) return null;
  const trimmed = raw.trim();
  if (trimmed.startsWith("tyde-pair://")) return trimmed;

  const hashIndex = trimmed.indexOf("#");
  if (hashIndex === -1) return null;
  const fragment = trimmed.slice(hashIndex + 1);
  if (fragment.startsWith("tyde-pair://")) return fragment;

  // Some scanners/links percent-encode the fragment; decode defensively.
  try {
    const decoded = decodeURIComponent(fragment);
    if (decoded.startsWith("tyde-pair://")) return decoded;
  } catch {
    // Malformed percent-encoding — fall through to "not found".
  }
  return null;
}

// DoS ceilings. A real pairing URI is a few hundred chars; these bound the
// worst case without rejecting any legitimate payload.
export const MAX_URI_LEN = 4096;
const MAX_B64_LEN = 6144; // ~ MAX_CBOR_BYTES * 4/3, with slack
const MAX_RELEASE_VERSION_LEN = 256;
const MAX_CBOR_DEPTH = 32;
const MAX_CBOR_ITEMS = 4096;

// Unicode White_Space code points — the EXACT set Rust's `str::trim`
// (`char::is_whitespace`) strips: U+0009-000D, U+0020, U+0085, U+00A0, U+1680,
// U+2000-200A, U+2028, U+2029, U+202F, U+205F, U+3000. Notably this EXCLUDES
// U+FEFF (BOM), which JS's built-in `String.prototype.trim` *does* strip. Using
// JS `.trim()` here would make the loader more permissive than the
// authoritative Rust parser (it would accept a trailing-BOM version that Rust
// rejects). We trim exactly Rust's set so validation is never looser than the
// host's.
const WS_CLASS =
  "[\\u0009-\\u000D\\u0020\\u0085\\u00A0\\u1680\\u2000-\\u200A\\u2028\\u2029\\u202F\\u205F\\u3000]";
const RUST_WS = new RegExp(`^${WS_CLASS}+|${WS_CLASS}+$`, "g");

function rustTrim(value) {
  return value.replace(RUST_WS, "");
}

// Decodes a URL-safe base64 (no padding) string to bytes. Rejects any
// character outside the base64url alphabet so malformed QR text fails loudly
// instead of being silently coerced.
export function base64urlToBytes(input) {
  if (typeof input !== "string") {
    throw new Error("base64url payload must be a string");
  }
  if (input.length > MAX_B64_LEN) {
    throw new Error(`base64url payload exceeds ${MAX_B64_LEN}-char cap`);
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
//   - trim (Rust's Unicode White_Space set), then strip a single leading `v`
//   - must not be empty
//   - must not exceed MAX_RELEASE_VERSION_LEN (DoS guard; Rust has no explicit
//     cap but real versions are tiny — a generous ceiling can't reject one)
//   - must not contain `/` or `\` (path separators)
//   - must not contain any whitespace
//   - core must be exactly `major.minor.patch`, each part non-empty ASCII digits
//   - optional prerelease (after the first `-`): non-empty, each dot-separated
//     identifier non-empty and limited to ASCII letters, digits, and `-`
export function validateReleaseVersion(raw) {
  if (typeof raw !== "string") return null;
  if (raw.length > MAX_RELEASE_VERSION_LEN) return null;
  let value = rustTrim(raw);
  if (value.startsWith("v")) value = value.slice(1);

  if (value.length === 0) return null;
  if (value.includes("/") || value.includes("\\")) return null;
  // Reject anything in Rust's whitespace set OR JS `\s` (incl. U+FEFF): the
  // grammar below would catch embedded whitespace anyway, but be explicit.
  if (/\s/.test(value) || new RegExp(WS_CLASS).test(value)) return null;

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

function pairingPrefix(uri) {
  for (const prefix of SUPPORTED_PAIRING_PREFIXES) {
    if (uri.startsWith(prefix)) return prefix;
  }

  const match = /^tyde-pair:\/\/v([^?]+)\?/.exec(uri);
  if (match) {
    throw new Error(`unsupported tyde-pair URI version v${match[1]}`);
  }
  throw new Error("not a supported tyde-pair pairing URI");
}

function decodePairingLoaderFields(bytes) {
  const decoder = new PairingCborFieldDecoder(bytes);
  return decoder.readTopLevelFields();
}

class PairingCborFieldDecoder {
  constructor(bytes) {
    this.bytes = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
    if (this.bytes.length > MAX_CBOR_BYTES) {
      throw new Error(`CBOR: input exceeds ${MAX_CBOR_BYTES}-byte cap`);
    }
    this.pos = 0;
    this.items = 0;
  }

  readTopLevelFields() {
    const header = this.readHeader(0);
    if (header.major !== 5) {
      this.skipBody(header, 0);
      throw new Error("pairing payload is not a CBOR map");
    }

    const len = this.readArg(header.info);
    if (len > MAX_CBOR_ITEMS) {
      throw new Error(`CBOR: item count exceeds ${MAX_CBOR_ITEMS} cap`);
    }

    let v = null;
    let protocolVersion = null;
    let releaseVersionRaw = null;
    for (let i = 0; i < len; i++) {
      const key = this.readTextOrNull(1);
      if (key === "v") {
        v = this.readIntegerOrNull(1);
      } else if (key === "protocol_version") {
        protocolVersion = this.readIntegerOrNull(1);
      } else if (key === "release_version") {
        releaseVersionRaw = this.readTextOrNull(1);
      } else {
        this.skipItem(1);
      }
    }

    return { v, protocolVersion, releaseVersionRaw };
  }

  readHeader(depth) {
    if (depth > MAX_CBOR_DEPTH) {
      throw new Error(`CBOR: nesting exceeds ${MAX_CBOR_DEPTH} levels`);
    }
    if (++this.items > MAX_CBOR_ITEMS) {
      throw new Error(`CBOR: item count exceeds ${MAX_CBOR_ITEMS} cap`);
    }
    const initial = this.byte();
    return { major: initial >> 5, info: initial & 0x1f };
  }

  byte() {
    if (this.pos >= this.bytes.length) {
      throw new Error("CBOR: unexpected end of input");
    }
    return this.bytes[this.pos++];
  }

  take(n) {
    if (n < 0 || this.pos + n > this.bytes.length) {
      throw new Error("CBOR: length exceeds input");
    }
    const slice = this.bytes.subarray(this.pos, this.pos + n);
    this.pos += n;
    return slice;
  }

  readArg(info) {
    if (info < 24) return info;
    if (info === 24) return this.byte();
    if (info === 25) {
      const b = this.take(2);
      return (b[0] << 8) | b[1];
    }
    if (info === 26) {
      const b = this.take(4);
      return b[0] * 0x1000000 + ((b[1] << 16) | (b[2] << 8) | b[3]);
    }
    if (info === 27) {
      const b = this.take(8);
      const hi = b[0] * 0x1000000 + ((b[1] << 16) | (b[2] << 8) | b[3]);
      const lo = b[4] * 0x1000000 + ((b[5] << 16) | (b[6] << 8) | b[7]);
      return hi * 0x100000000 + lo;
    }
    throw new Error("CBOR: indefinite or reserved length is not supported");
  }

  readTextOrNull(depth) {
    const header = this.readHeader(depth);
    if (header.major !== 3) {
      this.skipBody(header, depth);
      return null;
    }
    const len = this.readArg(header.info);
    return new TextDecoder("utf-8", { fatal: true }).decode(this.take(len));
  }

  readIntegerOrNull(depth) {
    const header = this.readHeader(depth);
    if (header.major === 0) return this.readArg(header.info);
    if (header.major === 1) return -1 - this.readArg(header.info);
    this.skipBody(header, depth);
    return null;
  }

  skipItem(depth) {
    const header = this.readHeader(depth);
    this.skipBody(header, depth);
  }

  skipBody(header, depth) {
    switch (header.major) {
      case 0:
      case 1:
        this.readArg(header.info);
        return;
      case 2:
      case 3:
        this.take(this.readArg(header.info));
        return;
      case 4: {
        const len = this.readArg(header.info);
        if (len > MAX_CBOR_ITEMS) {
          throw new Error(`CBOR: item count exceeds ${MAX_CBOR_ITEMS} cap`);
        }
        for (let i = 0; i < len; i++) this.skipItem(depth + 1);
        return;
      }
      case 5: {
        const len = this.readArg(header.info);
        if (len > MAX_CBOR_ITEMS) {
          throw new Error(`CBOR: item count exceeds ${MAX_CBOR_ITEMS} cap`);
        }
        for (let i = 0; i < len; i++) {
          this.skipItem(depth + 1);
          this.skipItem(depth + 1);
        }
        return;
      }
      case 6:
        this.readArg(header.info);
        this.skipItem(depth + 1);
        return;
      case 7:
        this.skipSimple(header.info);
        return;
      default:
        throw new Error("CBOR: unknown major type");
    }
  }

  skipSimple(info) {
    if (info < 24) return;
    if (info === 24) {
      this.take(1);
      return;
    }
    if (info === 25) {
      this.take(2);
      return;
    }
    if (info === 26) {
      this.take(4);
      return;
    }
    if (info === 27) {
      this.take(8);
      return;
    }
    throw new Error("CBOR: unsupported simple value or break code");
  }
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
  if (uri.length > MAX_URI_LEN) {
    throw new Error(`pairing URI exceeds ${MAX_URI_LEN}-char cap`);
  }
  const trimmed = uri.trim();
  const prefix = pairingPrefix(trimmed);
  const encoded = trimmed.slice(prefix.length);
  if (encoded.length === 0) {
    throw new Error("pairing payload is empty");
  }

  const bytes = base64urlToBytes(encoded);
  const { v, protocolVersion, releaseVersionRaw } = decodePairingLoaderFields(bytes);
  const releaseVersion = validateReleaseVersion(releaseVersionRaw);

  return { v, protocolVersion, releaseVersion, releaseVersionRaw };
}
