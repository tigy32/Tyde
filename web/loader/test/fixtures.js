// Test fixtures for the loader.
//
// The three REAL_* URIs below were produced by the authoritative Rust type
// (`MobilePairingQrPayload::to_uri`) via `cargo run -p mqtt-transport
// --example print_qr`. They are genuine host output, so the JS CBOR reader is
// tested against real ciborium encoding — not a JS re-implementation of it.
//
// `makePairingUri` builds synthetic payloads for injection/abuse cases that a
// real (validating) host would never emit, so we can prove the loader rejects
// them. It encodes a minimal CBOR map (text keys, uint/text values).

export const REAL_WITH_PRERELEASE =
  "tyde-pair://v1?qWF2AnBwcm90b2NvbF92ZXJzaW9uDWx0eWRlX3ZlcnNpb26jZW1ham9yAGVtaW5vcghlcGF0Y2gOZmJyb2tlcqJjdXJseB53c3M6Ly9icm9rZXIuZW1xeC5pbzo4MDg0L21xdHRkYXV0aKFka2luZGlhbm9ueW1vdXNmcG9saWN5pGxtcXR0X3ZlcnNpb24FY3FvcwFmcmV0YWlu9GtjbGVhbl9zdGFydPVkcm9vbXZCd2NIQndjSEJ3Y0hCd2NIQndjSEJ3Y3Bza1ggCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQlqaG9zdF9sYWJlbGx3aXRoX3JlbGVhc2VvcmVsZWFzZV92ZXJzaW9ubTAuOC4xOS1iZXRhLjI";

export const REAL_STABLE =
  "tyde-pair://v1?qWF2AnBwcm90b2NvbF92ZXJzaW9uDWx0eWRlX3ZlcnNpb26jZW1ham9yAGVtaW5vcghlcGF0Y2gOZmJyb2tlcqJjdXJseB53c3M6Ly9icm9rZXIuZW1xeC5pbzo4MDg0L21xdHRkYXV0aKFka2luZGlhbm9ueW1vdXNmcG9saWN5pGxtcXR0X3ZlcnNpb24FY3FvcwFmcmV0YWlu9GtjbGVhbl9zdGFydPVkcm9vbXZCd2NIQndjSEJ3Y0hCd2NIQndjSEJ3Y3Bza1ggCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQlqaG9zdF9sYWJlbG5zdGFibGVfcmVsZWFzZW9yZWxlYXNlX3ZlcnNpb25mMC44LjE5";

export const REAL_NO_RELEASE =
  "tyde-pair://v1?qGF2AnBwcm90b2NvbF92ZXJzaW9uDWx0eWRlX3ZlcnNpb26jZW1ham9yAGVtaW5vcghlcGF0Y2gOZmJyb2tlcqJjdXJseB53c3M6Ly9icm9rZXIuZW1xeC5pbzo4MDg0L21xdHRkYXV0aKFka2luZGlhbm9ueW1vdXNmcG9saWN5pGxtcXR0X3ZlcnNpb24FY3FvcwFmcmV0YWlu9GtjbGVhbl9zdGFydPVkcm9vbXZCd2NIQndjSEJ3Y0hCd2NIQndjSEJ3Y3Bza1ggCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQlqaG9zdF9sYWJlbGpub19yZWxlYXNl";

// --- synthetic CBOR map builder (text keys; uint or text values) -----------

function encodeUint(n) {
  if (n < 24) return [n];
  if (n < 256) return [0x18, n];
  if (n < 65536) return [0x19, n >> 8, n & 0xff];
  throw new Error("uint too large for fixture builder");
}

function encodeTextHeader(major, len) {
  const base = major << 5;
  if (len < 24) return [base | len];
  if (len < 256) return [base | 24, len];
  if (len < 65536) return [base | 25, len >> 8, len & 0xff];
  throw new Error("string too long for fixture builder");
}

function encodeText(str) {
  const bytes = Array.from(new TextEncoder().encode(str));
  return [...encodeTextHeader(3, bytes.length), ...bytes];
}

function encodeValue(value) {
  if (typeof value === "number") return encodeUint(value);
  if (typeof value === "string") return encodeText(value);
  throw new Error("fixture builder supports only uint/text values");
}

// Builds a `tyde-pair://v1?` URI from a flat object of string/number entries.
// Insertion order is preserved, mirroring how a CBOR map is laid out.
export function makePairingUri(entries) {
  const keys = Object.keys(entries);
  const bytes = [...encodeTextHeader(5, keys.length)]; // major 5 = map
  for (const key of keys) {
    bytes.push(...encodeText(key));
    bytes.push(...encodeValue(entries[key]));
  }
  const b64 = Buffer.from(Uint8Array.from(bytes))
    .toString("base64")
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
  return "tyde-pair://v1?" + b64;
}

export const EXAMPLE_MANIFEST = {
  schemaVersion: 1,
  minSupported: "0.8.19-beta.1",
  blocked: ["0.8.18"],
  versions: {
    "0.8.19-beta.2": {
      path: "/tyde/v0.8.19-beta.2/",
      entry: "/tyde/v0.8.19-beta.2/tyde-mobile.js",
      integrity: "sha384-" + "A".repeat(64),
      artifacts: {
        "/tyde/v0.8.19-beta.2/tyde-mobile_bg.wasm": "sha384-" + "C".repeat(64),
      },
    },
    "0.8.19": {
      path: "/tyde/v0.8.19/",
      entry: "/tyde/v0.8.19/tyde-mobile.js",
      integrity: "sha384-" + "B".repeat(64),
      artifacts: {
        "/tyde/v0.8.19/tyde-mobile_bg.wasm": "sha384-" + "D".repeat(64),
      },
    },
  },
};
