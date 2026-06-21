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
// path it hands back is a same-origin, traversal-free `/tyde/...` path. Rejects
// raw `..`/`\`, percent-encoded traversal (`%2e`, `%2f`, `%5c` in any case),
// and any path that — resolved as a URL — escapes the origin or the `/tyde/`
// prefix.
function isSafeBundlePath(path) {
  if (typeof path !== "string") return false;
  if (!path.startsWith("/tyde/")) return false;
  if (path.includes("..") || path.includes("\\")) return false;
  if (/%2e|%2f|%5c/i.test(path)) return false;
  if (/\s/.test(path)) return false;
  // Resolve against a fixed sentinel origin; a path that escapes it (e.g.
  // protocol-relative `//evil`, or backslash tricks) lands on another origin.
  let resolved;
  try {
    resolved = new URL(path, "https://loader.invalid");
  } catch {
    return false;
  }
  return (
    resolved.origin === "https://loader.invalid" &&
    resolved.pathname.startsWith("/tyde/")
  );
}

function isValidIntegrity(value) {
  return typeof value === "string" && INTEGRITY_RE.test(value);
}

// Resolves the boot target for a version against the manifest. Returns either
// `{ ok: true, version, path, entry, integrity, artifacts }` (artifacts is the
// full list of `{ url, integrity }` that must be SRI-verified before the bundle
// runs — entry JS first, then wasm + chunks) or `{ ok: false, reason }` where
// reason is one of:
//   invalid-version | no-manifest | bad-policy | blocked | below-min-supported |
//   not-in-manifest | bad-entry-path | bad-integrity
//
// FAIL-CLOSED: a manifest whose POLICY fields are malformed (non-array
// `blocked`, or a present-but-invalid `minSupported`) is rejected wholesale
// rather than silently degraded — a corrupted/partial manifest must never widen
// what is allowed to boot.
export function resolveBootTarget(version, manifest) {
  const norm = validateReleaseVersion(version);
  if (!norm) return { ok: false, reason: "invalid-version" };
  if (!manifest || typeof manifest !== "object") {
    return { ok: false, reason: "no-manifest" };
  }

  // `blocked`: must be absent or an array. A non-array (object/string/number)
  // is a malformed manifest → fail closed.
  if (manifest.blocked !== undefined && !Array.isArray(manifest.blocked)) {
    return { ok: false, reason: "bad-policy" };
  }
  const blocked = Array.isArray(manifest.blocked) ? manifest.blocked : [];
  // Normalize each blocked entry through the same validator so e.g. `v0.8.18`
  // and `0.8.18` (or padded variants) match the normalized `norm`.
  for (const raw of blocked) {
    const normalizedBlock = validateReleaseVersion(raw);
    if (normalizedBlock && normalizedBlock === norm) {
      return { ok: false, reason: "blocked" };
    }
  }

  // `minSupported`: must be absent or a VALID version. A present-but-invalid
  // floor is a malformed manifest → fail closed (do NOT ignore it).
  if (manifest.minSupported !== undefined) {
    if (typeof manifest.minSupported !== "string") {
      return { ok: false, reason: "bad-policy" };
    }
    const min = validateReleaseVersion(manifest.minSupported);
    if (!min) return { ok: false, reason: "bad-policy" };
    if (compareVersions(norm, min) < 0) {
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
  if (!isValidIntegrity(entry.integrity)) {
    return { ok: false, reason: "bad-integrity" };
  }

  // Build the full executable-artifact list. The entry JS is implicit; every
  // additional executable artifact (the wasm and any code-split chunks) MUST be
  // listed in `entry.artifacts` as `{ "<path>": "<integrity>" }` so it can be
  // SRI-verified before the bundle runs. A tampered same-version `.wasm` is the
  // gap the entry-only `<script integrity>` left open.
  const artifacts = [{ url: target, integrity: entry.integrity }];
  if (entry.artifacts !== undefined) {
    if (typeof entry.artifacts !== "object" || Array.isArray(entry.artifacts)) {
      return { ok: false, reason: "bad-integrity" };
    }
    for (const [url, integrity] of Object.entries(entry.artifacts)) {
      if (!isSafeBundlePath(url)) return { ok: false, reason: "bad-entry-path" };
      if (!isValidIntegrity(integrity)) {
        return { ok: false, reason: "bad-integrity" };
      }
      artifacts.push({ url, integrity });
    }
  }

  return {
    ok: true,
    version: norm,
    path: typeof entry.path === "string" ? entry.path : target,
    entry: target,
    integrity: entry.integrity,
    artifacts,
  };
}

// Selects the two URLs the loader dynamically imports to boot a Trunk bundle:
// the entry ES module (`entryUrl`) and the explicit hashed `…_bg.wasm` the
// module's default `init()` must be called with (`wasmUrl`).
//
// WHY this is needed: Trunk's wasm-bindgen entry JS only EXPORTS its init
// (`export { __wbg_init as default }`) — it does NOT auto-run. And the entry's
// built-in default wasm path (from `import.meta.url`) is the UNHASHED
// `…_bg.wasm`, which does not exist (the real file is content-hashed). So the
// loader must dynamically `import()` the entry and call its `init()` with the
// real hashed wasm path explicitly.
//
// Both URLs come straight from the already-validated, already-SRI-verified
// `target` (its `entry` and `artifacts`), NOT from the QR or a re-fetched
// index.html — so the dynamic `import()`/`init()` read exactly the bytes
// integrity.js verified and cached. Defense in depth: each URL is re-confined to
// the version's own `/tyde/v<ver>/` directory (`target.path`) and screened for
// traversal, even though resolveBootTarget already vetted them. Returns
// `{ ok: true, entryUrl, wasmUrl }` or `{ ok: false, reason }` where reason is
// `bad-target` | `bad-entry-path` | `no-wasm-artifact`.
export function selectBootUrls(target) {
  if (!target || typeof target !== "object") {
    return { ok: false, reason: "bad-target" };
  }
  const versionPath = typeof target.path === "string" ? target.path : null;
  if (!versionPath || !versionPath.startsWith("/tyde/")) {
    return { ok: false, reason: "bad-target" };
  }
  const underVersion = (url) =>
    typeof url === "string" &&
    url.startsWith(versionPath) &&
    !url.includes("..") &&
    !url.includes("\\") &&
    !/%2e|%2f|%5c/i.test(url);

  const entryUrl = target.entry;
  if (!underVersion(entryUrl)) return { ok: false, reason: "bad-entry-path" };

  const wasm = Array.isArray(target.artifacts)
    ? target.artifacts.find(
        (a) => a && typeof a.url === "string" && a.url.endsWith("_bg.wasm"),
      )
    : null;
  if (!wasm || !underVersion(wasm.url)) {
    return { ok: false, reason: "no-wasm-artifact" };
  }

  return { ok: true, entryUrl, wasmUrl: wasm.url };
}
