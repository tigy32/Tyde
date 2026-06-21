import { test } from "node:test";
import assert from "node:assert/strict";

import { decodeFirst } from "../cbor.js";
import {
  parsePairingUri,
  validateReleaseVersion,
  base64urlToBytes,
} from "../pairing.js";
import { resolveBootTarget, compareVersions } from "../manifest-policy.js";
import {
  REAL_WITH_PRERELEASE,
  REAL_STABLE,
  REAL_NO_RELEASE,
  makePairingUri,
  EXAMPLE_MANIFEST,
} from "./fixtures.js";

// --- CBOR reader -----------------------------------------------------------

test("cbor decodes a real ciborium-encoded pairing payload to a map", () => {
  const encoded = REAL_WITH_PRERELEASE.slice("tyde-pair://v1?".length);
  const map = decodeFirst(base64urlToBytes(encoded));
  assert.equal(typeof map, "object");
  assert.equal(map.v, 2);
  assert.equal(map.protocol_version, 13);
  assert.equal(map.release_version, "0.8.19-beta.2");
  // Nested struct (tyde_version) is walked, not just skipped.
  assert.equal(map.tyde_version.minor, 8);
});

test("cbor rejects indefinite-length items", () => {
  // 0x5f = byte string, indefinite length.
  assert.throws(() => decodeFirst(Uint8Array.from([0x5f, 0xff])));
});

// --- release_version validation (mirror of TydeReleaseVersion) -------------

test("validateReleaseVersion accepts valid stable and prerelease versions", () => {
  assert.equal(validateReleaseVersion("0.8.19"), "0.8.19");
  assert.equal(validateReleaseVersion("0.8.20-beta.1"), "0.8.20-beta.1");
  assert.equal(validateReleaseVersion("10.0.0-rc-1"), "10.0.0-rc-1");
  // leading `v` is stripped, like the Rust parser.
  assert.equal(validateReleaseVersion("v1.2.3"), "1.2.3");
  // surrounding whitespace is trimmed.
  assert.equal(validateReleaseVersion("  0.8.19  "), "0.8.19");
});

test("validateReleaseVersion rejects injection / malformed input", () => {
  const bad = [
    "../evil",
    "..\\evil",
    "0.8.19/../0.8.20",
    "0.8.19\\x",
    "0.8 .19", // internal whitespace (trailing is trimmed, like Rust)
    "", // handled after trim
    "   ", // trims to empty
    "1.2", // too few core parts
    "1.2.3.4", // too many
    "1.2.x", // non-numeric core
    "1..3", // empty core part
    "1.2.3-", // empty prerelease
    "1.2.3-beta..1", // empty prerelease identifier
    "1.2.3-beta!", // illegal prerelease char
    "01.2.3-✓", // non-ascii
  ];
  for (const value of bad) {
    assert.equal(validateReleaseVersion(value), null, `expected reject: ${value}`);
  }
});

// --- parsePairingUri --------------------------------------------------------

test("parsePairingUri extracts version from real host URIs", () => {
  assert.equal(parsePairingUri(REAL_WITH_PRERELEASE).releaseVersion, "0.8.19-beta.2");
  assert.equal(parsePairingUri(REAL_STABLE).releaseVersion, "0.8.19");
  const none = parsePairingUri(REAL_NO_RELEASE);
  assert.equal(none.releaseVersion, null);
  assert.equal(none.v, 2);
});

test("parsePairingUri rejects non-pairing and malformed URIs", () => {
  assert.throws(() => parsePairingUri("https://evil.example/"));
  assert.throws(() => parsePairingUri("tyde-pair://v1?")); // empty payload
  assert.throws(() => parsePairingUri("tyde-pair://v1?!!!not-base64!!!"));
  assert.throws(() => parsePairingUri(42));
});

test("parsePairingUri returns null version for embedded injection strings", () => {
  // A hand-crafted (non-host) QR carrying a traversal string is parsed, but the
  // version fails validation and comes back null — it can never reach a URL.
  const evil = makePairingUri({ v: 2, protocol_version: 13, release_version: "../evil" });
  const parsed = parsePairingUri(evil);
  assert.equal(parsed.releaseVersionRaw, "../evil");
  assert.equal(parsed.releaseVersion, null);

  const slashed = makePairingUri({
    v: 2,
    release_version: "/tyde/v9.9.9/../../evil",
  });
  assert.equal(parsePairingUri(slashed).releaseVersion, null);
});

// --- compareVersions --------------------------------------------------------

test("compareVersions orders core and prerelease correctly", () => {
  assert.equal(compareVersions("0.8.19", "0.8.19"), 0);
  assert.equal(compareVersions("0.8.18", "0.8.19"), -1);
  assert.equal(compareVersions("0.9.0", "0.8.99"), 1);
  // prerelease sorts before its release
  assert.equal(compareVersions("0.8.19-beta.1", "0.8.19"), -1);
  assert.equal(compareVersions("0.8.19-beta.1", "0.8.19-beta.2"), -1);
  assert.equal(compareVersions("0.8.19-beta.2", "0.8.19-beta.10"), -1);
});

// --- resolveBootTarget (manifest is the authority) -------------------------

test("resolveBootTarget boots a version present in the manifest", () => {
  const r = resolveBootTarget("0.8.19-beta.2", EXAMPLE_MANIFEST);
  assert.equal(r.ok, true);
  assert.equal(r.version, "0.8.19-beta.2");
  assert.equal(r.entry, "/tyde/v0.8.19-beta.2/tyde-mobile.js");
  assert.match(r.integrity, /^sha384-/);
});

test("resolveBootTarget gates booting on manifest membership", () => {
  // Valid semver, but not published -> rejected.
  assert.deepEqual(resolveBootTarget("99.99.99", EXAMPLE_MANIFEST), {
    ok: false,
    reason: "not-in-manifest",
  });
});

test("resolveBootTarget rejects invalid versions before lookup", () => {
  assert.equal(resolveBootTarget("../evil", EXAMPLE_MANIFEST).reason, "invalid-version");
  assert.equal(resolveBootTarget("0.8.19/x", EXAMPLE_MANIFEST).reason, "invalid-version");
  assert.equal(resolveBootTarget("not-semver", EXAMPLE_MANIFEST).reason, "invalid-version");
});

test("resolveBootTarget enforces blocked list and minSupported floor", () => {
  const manifest = {
    ...EXAMPLE_MANIFEST,
    blocked: ["0.8.19-beta.2"],
    versions: {
      ...EXAMPLE_MANIFEST.versions,
      "0.8.17": {
        path: "/tyde/v0.8.17/",
        entry: "/tyde/v0.8.17/tyde-mobile.js",
        integrity: "sha384-" + "C".repeat(64),
      },
    },
  };
  // Explicitly blocked even though it is in `versions`.
  assert.equal(resolveBootTarget("0.8.19-beta.2", manifest).reason, "blocked");
  // Below minSupported (0.8.19-beta.1) even though it is in `versions`.
  assert.equal(resolveBootTarget("0.8.17", manifest).reason, "below-min-supported");
});

test("resolveBootTarget rejects malformed manifest entries (defense in depth)", () => {
  const badPath = {
    versions: {
      "1.2.3": { path: "/tyde/v1.2.3/", entry: "/tyde/v1.2.3/../../evil.js", integrity: "sha384-" + "A".repeat(64) },
    },
  };
  assert.equal(resolveBootTarget("1.2.3", badPath).reason, "bad-entry-path");

  const offOrigin = {
    versions: {
      "1.2.3": { path: "/tyde/v1.2.3/", entry: "https://evil.example/x.js", integrity: "sha384-" + "A".repeat(64) },
    },
  };
  assert.equal(resolveBootTarget("1.2.3", offOrigin).reason, "bad-entry-path");

  const badIntegrity = {
    versions: {
      "1.2.3": { path: "/tyde/v1.2.3/", entry: "/tyde/v1.2.3/app.js", integrity: "md5-nope" },
    },
  };
  assert.equal(resolveBootTarget("1.2.3", badIntegrity).reason, "bad-integrity");
});

test("resolveBootTarget handles a missing manifest", () => {
  assert.equal(resolveBootTarget("0.8.19", null).reason, "no-manifest");
});

// --- returning-user path (end to end of the pure logic) --------------------

test("returning-user flow: stored version re-resolves via the manifest", () => {
  // Simulates loader.init()'s fast path: a remembered version is re-checked
  // against the freshly fetched manifest before booting with no QR.
  const remembered = "0.8.19";
  const r = resolveBootTarget(remembered, EXAMPLE_MANIFEST);
  assert.equal(r.ok, true);
  assert.equal(r.entry, "/tyde/v0.8.19/tyde-mobile.js");

  // If the manifest later drops/blocks that version, the fast path must fail
  // closed so the loader falls back to re-pairing.
  const dropped = { ...EXAMPLE_MANIFEST, versions: { "0.8.19-beta.2": EXAMPLE_MANIFEST.versions["0.8.19-beta.2"] } };
  assert.equal(resolveBootTarget(remembered, dropped).ok, false);
});
