// Manifest allowlist + version policy. Pure module (no DOM / no network).
//
// The manifest is the ONLY authority for which versions may boot. The loader
// never constructs a bundle URL by concatenating the (attacker-influenced) QR
// version with a path. Instead it looks the validated version up in the
// manifest and uses the manifest's OWN `entry`/`path` strings. This module also
// enforces a `minSupported` floor and an explicit `blocked` list so a
// compromised or downgraded host cannot force a known-bad client.

import { validateReleaseVersion } from "./pairing.js";

// Compares two validated version strings using semver-lite ordering:
// numeric core compared field-by-field; a version WITH a prerelease sorts
// BEFORE the same version without one; prerelease identifiers compared
// numerically when both numeric, else lexically. Returns -1, 0, or 1.
export function compareVersions(a, b) {
  const pa = splitVersion(a);
  const pb = splitVersion(b);
  for (let i = 0; i < 3; i++) {
    if (pa.core[i] !== pb.core[i]) return pa.core[i] < pb.core[i] ? -1 : 1;
  }
  if (pa.pre === null && pb.pre === null) return 0;
  if (pa.pre === null) return 1; // no prerelease > has prerelease
  if (pb.pre === null) return -1;
  const ai = pa.pre.split(".");
  const bi = pb.pre.split(".");
  const len = Math.max(ai.length, bi.length);
  for (let i = 0; i < len; i++) {
    if (i >= ai.length) return -1;
    if (i >= bi.length) return 1;
    const cmp = compareIdentifier(ai[i], bi[i]);
    if (cmp !== 0) return cmp;
  }
  return 0;
}

function splitVersion(v) {
  const dash = v.indexOf("-");
  const core = (dash === -1 ? v : v.slice(0, dash)).split(".").map(Number);
  const pre = dash === -1 ? null : v.slice(dash + 1);
  return { core, pre };
}

function compareIdentifier(a, b) {
  const an = /^[0-9]+$/.test(a);
  const bn = /^[0-9]+$/.test(b);
  if (an && bn) {
    const x = Number(a);
    const y = Number(b);
    return x === y ? 0 : x < y ? -1 : 1;
  }
  if (an) return -1; // numeric identifiers have lower precedence than alphanumeric
  if (bn) return 1;
  return a === b ? 0 : a < b ? -1 : 1;
}

// SRI integrity strings must name a supported hash and carry a base64 digest.
const INTEGRITY_RE = /^sha(256|384|512)-[A-Za-z0-9+/]+={0,2}$/;

// Defense in depth: even though the manifest is server-controlled, confirm the
// path it hands back is a same-origin, traversal-free `/tyde/...` path.
function isSafeBundlePath(path) {
  return (
    typeof path === "string" &&
    path.startsWith("/tyde/") &&
    !path.includes("..") &&
    !path.includes("\\") &&
    !/\s/.test(path)
  );
}

// Resolves the boot target for a version against the manifest. Returns either
// `{ ok: true, version, path, entry, integrity }` or
// `{ ok: false, reason }` where reason is one of:
//   invalid-version | no-manifest | blocked | below-min-supported |
//   not-in-manifest | bad-entry-path | bad-integrity
export function resolveBootTarget(version, manifest) {
  const norm = validateReleaseVersion(version);
  if (!norm) return { ok: false, reason: "invalid-version" };
  if (!manifest || typeof manifest !== "object") {
    return { ok: false, reason: "no-manifest" };
  }

  const blocked = Array.isArray(manifest.blocked) ? manifest.blocked : [];
  if (blocked.includes(norm)) return { ok: false, reason: "blocked" };

  if (typeof manifest.minSupported === "string") {
    const min = validateReleaseVersion(manifest.minSupported);
    if (min && compareVersions(norm, min) < 0) {
      return { ok: false, reason: "below-min-supported" };
    }
  }

  const versions =
    manifest.versions && typeof manifest.versions === "object"
      ? manifest.versions
      : {};
  const entry = Object.prototype.hasOwnProperty.call(versions, norm)
    ? versions[norm]
    : null;
  if (!entry || typeof entry !== "object") {
    return { ok: false, reason: "not-in-manifest" };
  }

  const target = typeof entry.entry === "string" ? entry.entry : entry.path;
  if (!isSafeBundlePath(target)) return { ok: false, reason: "bad-entry-path" };
  if (typeof entry.path === "string" && !isSafeBundlePath(entry.path)) {
    return { ok: false, reason: "bad-entry-path" };
  }
  if (typeof entry.integrity !== "string" || !INTEGRITY_RE.test(entry.integrity)) {
    return { ok: false, reason: "bad-integrity" };
  }

  return {
    ok: true,
    version: norm,
    path: typeof entry.path === "string" ? entry.path : target,
    entry: target,
    integrity: entry.integrity,
  };
}
