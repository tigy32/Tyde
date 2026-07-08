import { test } from "node:test";
import assert from "node:assert/strict";

import { decodeFirst } from "../cbor.js";
import {
  parsePairingUri,
  validateReleaseVersion,
  base64urlToBytes,
  extractPairingUri,
  MAX_URI_LEN,
} from "../pairing.js";
import {
  resolveBootTarget,
  resolveLatestBootTarget,
  compareVersions,
  selectBootUrls,
} from "../manifest-policy.js";
import {
  REAL_WITH_PRERELEASE,
  REAL_STABLE,
  REAL_NO_RELEASE,
  REAL_MANAGED_V2,
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

test("parsePairingUri extracts loader fields from real managed v2 URIs", () => {
  const parsed = parsePairingUri(REAL_MANAGED_V2);
  assert.deepEqual(Object.keys(parsed).sort(), [
    "protocolVersion",
    "releaseVersion",
    "releaseVersionRaw",
    "v",
  ]);
  assert.equal(parsed.v, 3);
  assert.equal(parsed.protocolVersion, 37);
  assert.equal(parsed.releaseVersion, "0.8.19");
  assert.equal(parsed.releaseVersionRaw, "0.8.19");
  assert.equal(Object.hasOwn(parsed, "offer_secret"), false);
  assert.equal(Object.hasOwn(parsed, "broker"), false);
});

test("parsePairingUri rejects non-pairing and malformed URIs", () => {
  assert.throws(() => parsePairingUri("https://evil.example/"));
  assert.throws(() => parsePairingUri("tyde-pair://v1?")); // empty payload
  assert.throws(() => parsePairingUri("tyde-pair://v2?")); // empty payload
  assert.throws(() => parsePairingUri("tyde-pair://v1?!!!not-base64!!!"));
  assert.throws(
    () =>
      parsePairingUri(
        REAL_MANAGED_V2.replace("tyde-pair://v2?", "tyde-pair://v3?"),
      ),
    /unsupported tyde-pair URI version v3/,
  );
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

// --- extractPairingUri (generic HTTPS QR normalization) --------------------

test("extractPairingUri returns the raw tyde-pair:// URI unchanged", () => {
  assert.equal(extractPairingUri(REAL_STABLE), REAL_STABLE);
  assert.equal(extractPairingUri(REAL_MANAGED_V2), REAL_MANAGED_V2);
  // Surrounding whitespace is trimmed.
  assert.equal(extractPairingUri(`  ${REAL_STABLE}  `), REAL_STABLE);
});

test("extractPairingUri pulls the inner URI out of the HTTPS fragment form", () => {
  const url = `https://tycode.dev/tyde/#${REAL_STABLE}`;
  assert.equal(extractPairingUri(url), REAL_STABLE);
  // With a path/query before the fragment.
  const withQuery = `https://tycode.dev/tyde/?utm=x#${REAL_WITH_PRERELEASE}`;
  assert.equal(extractPairingUri(withQuery), REAL_WITH_PRERELEASE);
  const managed = `https://tycode.dev/tyde/#${REAL_MANAGED_V2}`;
  assert.equal(extractPairingUri(managed), REAL_MANAGED_V2);
});

test("extractPairingUri decodes a percent-encoded fragment", () => {
  const encoded = `https://tycode.dev/tyde/#${encodeURIComponent(REAL_STABLE)}`;
  assert.equal(extractPairingUri(encoded), REAL_STABLE);
});

test("extractPairingUri rejects junk and non-pairing URLs", () => {
  assert.equal(extractPairingUri("https://evil.example/"), null);
  assert.equal(extractPairingUri("https://tycode.dev/tyde/#nothing-here"), null);
  assert.equal(extractPairingUri("just some text"), null);
  assert.equal(extractPairingUri(42), null);
  assert.equal(extractPairingUri(null), null);
  // Over-long input is rejected before any work.
  assert.equal(extractPairingUri("#tyde-pair://" + "A".repeat(MAX_URI_LEN)), null);
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

test("resolveBootTarget exposes a stamped protocolVersion", () => {
  const r = resolveBootTarget("0.8.19-beta.2", EXAMPLE_MANIFEST);
  assert.equal(r.ok, true);
  assert.equal(r.protocolVersion, 13);
});

test("resolveBootTarget exposes protocolVersion=null when the entry predates the field", () => {
  const manifest = {
    versions: {
      "1.2.3": {
        path: "/tyde/v1.2.3/",
        entry: "/tyde/v1.2.3/app.js",
        integrity: "sha384-" + "A".repeat(64),
        artifacts: { "/tyde/v1.2.3/app_bg.wasm": "sha384-" + "B".repeat(64) },
      },
    },
  };
  const r = resolveBootTarget("1.2.3", manifest);
  assert.equal(r.ok, true);
  assert.equal(r.protocolVersion, null);
});

test("resolveBootTarget fails closed on a present-but-malformed protocolVersion", () => {
  const bad = (pv) => ({
    versions: {
      "1.2.3": {
        path: "/tyde/v1.2.3/",
        entry: "/tyde/v1.2.3/app.js",
        integrity: "sha384-" + "A".repeat(64),
        protocolVersion: pv,
        artifacts: { "/tyde/v1.2.3/app_bg.wasm": "sha384-" + "B".repeat(64) },
      },
    },
  });
  for (const pv of ["13", 1.5, -1, null, {}, NaN]) {
    assert.equal(
      resolveBootTarget("1.2.3", bad(pv)).reason,
      "bad-protocol-version",
      `expected bad-protocol-version for ${JSON.stringify(pv)}`,
    );
  }
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

test("resolveLatestBootTarget selects the newest bootable manifest version", () => {
  const integrity = "sha384-" + "A".repeat(64);
  const wasmIntegrity = "sha384-" + "B".repeat(64);
  const makeEntry = (version) => ({
    path: `/tyde/v${version}/`,
    entry: `/tyde/v${version}/app.js`,
    integrity,
    artifacts: { [`/tyde/v${version}/app_bg.wasm`]: wasmIntegrity },
  });
  const manifest = {
    schemaVersion: 1,
    minSupported: "0.8.19-beta.1",
    blocked: [],
    versions: {
      "0.8.19-beta.4": makeEntry("0.8.19-beta.4"),
      "0.8.19-beta.8": makeEntry("0.8.19-beta.8"),
    },
  };

  const latest = resolveLatestBootTarget(manifest);
  assert.equal(latest.ok, true);
  assert.equal(latest.version, "0.8.19-beta.8");

  const blockedLatest = { ...manifest, blocked: ["0.8.19-beta.8"] };
  const fallback = resolveLatestBootTarget(blockedLatest);
  assert.equal(fallback.ok, true);
  assert.equal(fallback.version, "0.8.19-beta.4");
});

// --- returning-user path (end to end of the pure logic) --------------------

// --- #2 artifact integrity coverage ----------------------------------------

test("resolveBootTarget returns the full artifact list (entry + wasm)", () => {
  const r = resolveBootTarget("0.8.19-beta.2", EXAMPLE_MANIFEST);
  assert.equal(r.ok, true);
  assert.equal(r.artifacts.length, 2);
  assert.equal(r.artifacts[0].url, "/tyde/v0.8.19-beta.2/tyde-mobile.js");
  assert.equal(r.artifacts[1].url, "/tyde/v0.8.19-beta.2/tyde-mobile_bg.wasm");
  for (const a of r.artifacts) assert.match(a.integrity, /^sha384-/);
});

test("resolveBootTarget rejects a bad artifact path or integrity", () => {
  const badArtifactPath = {
    versions: {
      "1.2.3": {
        path: "/tyde/v1.2.3/",
        entry: "/tyde/v1.2.3/app.js",
        integrity: "sha384-" + "A".repeat(64),
        artifacts: { "https://evil.example/x.wasm": "sha384-" + "B".repeat(64) },
      },
    },
  };
  assert.equal(resolveBootTarget("1.2.3", badArtifactPath).reason, "bad-entry-path");

  const badArtifactIntegrity = {
    versions: {
      "1.2.3": {
        path: "/tyde/v1.2.3/",
        entry: "/tyde/v1.2.3/app.js",
        integrity: "sha384-" + "A".repeat(64),
        artifacts: { "/tyde/v1.2.3/x.wasm": "md5-nope" },
      },
    },
  };
  assert.equal(resolveBootTarget("1.2.3", badArtifactIntegrity).reason, "bad-integrity");

  const artifactsNotObject = {
    versions: {
      "1.2.3": {
        path: "/tyde/v1.2.3/",
        entry: "/tyde/v1.2.3/app.js",
        integrity: "sha384-" + "A".repeat(64),
        artifacts: ["/tyde/v1.2.3/x.wasm"],
      },
    },
  };
  assert.equal(resolveBootTarget("1.2.3", artifactsNotObject).reason, "bad-integrity");
});

// --- selectBootUrls (Trunk-style dynamic-import boot) ----------------------

test("selectBootUrls picks the entry module + hashed _bg.wasm from the target", () => {
  const target = resolveBootTarget("0.8.19-beta.2", EXAMPLE_MANIFEST);
  assert.equal(target.ok, true);
  const urls = selectBootUrls(target);
  assert.deepEqual(urls, {
    ok: true,
    entryUrl: "/tyde/v0.8.19-beta.2/tyde-mobile.js",
    wasmUrl: "/tyde/v0.8.19-beta.2/tyde-mobile_bg.wasm",
  });
});

test("selectBootUrls fails closed when no _bg.wasm artifact is present", () => {
  const target = {
    version: "1.2.3",
    path: "/tyde/v1.2.3/",
    entry: "/tyde/v1.2.3/app.js",
    artifacts: [{ url: "/tyde/v1.2.3/app.js", integrity: "sha384-x" }],
  };
  assert.equal(selectBootUrls(target).reason, "no-wasm-artifact");
});

test("selectBootUrls re-confines both URLs to the version directory", () => {
  // Entry outside the version path → rejected.
  assert.equal(
    selectBootUrls({
      version: "1.2.3",
      path: "/tyde/v1.2.3/",
      entry: "/tyde/v9.9.9/app.js",
      artifacts: [{ url: "/tyde/v1.2.3/app_bg.wasm", integrity: "sha384-x" }],
    }).reason,
    "bad-entry-path",
  );
  // Wasm artifact outside the version path → rejected as if absent.
  assert.equal(
    selectBootUrls({
      version: "1.2.3",
      path: "/tyde/v1.2.3/",
      entry: "/tyde/v1.2.3/app.js",
      artifacts: [{ url: "/tyde/v9.9.9/app_bg.wasm", integrity: "sha384-x" }],
    }).reason,
    "no-wasm-artifact",
  );
});

test("selectBootUrls rejects a malformed target", () => {
  assert.equal(selectBootUrls(null).reason, "bad-target");
  assert.equal(selectBootUrls({ path: "/evil/v1/", entry: "/evil/v1/x.js" }).reason, "bad-target");
});

// --- #9 percent-encoded / off-origin path rejection -------------------------

test("isSafeBundlePath (via resolveBootTarget) rejects percent-encoded traversal", () => {
  for (const evil of [
    "/tyde/v1.2.3/%2e%2e/evil.js",
    "/tyde/v1.2.3/%2E%2E/evil.js",
    "/tyde/v1.2.3/x%2fevil.js",
    "/tyde/v1.2.3/x%5cevil.js",
  ]) {
    const m = {
      versions: {
        "1.2.3": { path: "/tyde/v1.2.3/", entry: evil, integrity: "sha384-" + "A".repeat(64) },
      },
    };
    assert.equal(resolveBootTarget("1.2.3", m).reason, "bad-entry-path", evil);
  }
});

// --- #4 fail-closed manifest policy -----------------------------------------

test("resolveBootTarget fails closed on a non-array blocked list", () => {
  const m = { ...EXAMPLE_MANIFEST, blocked: { "0.8.19-beta.2": true } };
  assert.equal(resolveBootTarget("0.8.19-beta.2", m).reason, "bad-policy");
});

test("resolveBootTarget fails closed on an invalid minSupported", () => {
  assert.equal(
    resolveBootTarget("0.8.19-beta.2", { ...EXAMPLE_MANIFEST, minSupported: "not-semver" }).reason,
    "bad-policy",
  );
  assert.equal(
    resolveBootTarget("0.8.19-beta.2", { ...EXAMPLE_MANIFEST, minSupported: 819 }).reason,
    "bad-policy",
  );
});

test("blocked entries are normalized through validateReleaseVersion", () => {
  // `v0.8.19-beta.2` must block the normalized `0.8.19-beta.2`.
  const m = { ...EXAMPLE_MANIFEST, blocked: ["v0.8.19-beta.2"] };
  assert.equal(resolveBootTarget("0.8.19-beta.2", m).reason, "blocked");
});

// --- #6 DoS caps + Rust-matching whitespace ---------------------------------

test("validateReleaseVersion enforces a max length", () => {
  assert.equal(validateReleaseVersion("0.8." + "9".repeat(300)), null);
});

test("validateReleaseVersion matches Rust whitespace semantics", () => {
  // Rust's str::trim strips U+0085 (NEL) and U+3000 (ideographic space) — so do we.
  assert.equal(validateReleaseVersion("0.8.19"), "0.8.19");
  assert.equal(validateReleaseVersion("　0.8.19"), "0.8.19");
  // U+FEFF (BOM) is NOT Unicode White_Space; Rust would keep it (then reject via
  // the grammar). We must NOT silently strip it and accept — fail closed.
  assert.equal(validateReleaseVersion("0.8.19﻿"), null);
});

test("parsePairingUri rejects an over-long URI", () => {
  const huge = "tyde-pair://v1?" + "A".repeat(MAX_URI_LEN);
  assert.throws(() => parsePairingUri(huge), /exceeds/);
});

test("cbor rejects over-deep nesting and oversized input", () => {
  // 40 nested single-element arrays (0x81) then 0x00 — exceeds the depth cap.
  const deep = new Uint8Array(41);
  deep.fill(0x81, 0, 40);
  deep[40] = 0x00;
  assert.throws(() => decodeFirst(deep), /nesting exceeds/);

  assert.throws(() => decodeFirst(new Uint8Array(5000)), /exceeds .*cap/);
});

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
