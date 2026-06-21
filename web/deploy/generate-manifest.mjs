#!/usr/bin/env node
// Tyde web release manifest generator (Phase 6).
//
// Given a built Trunk `dist/` and a release version, emit/merge the
// `manifest.json` the loader's allowlist authority expects:
//
//   {
//     "schemaVersion": 1,
//     "minSupported": "<ver>",          // preserved/overridable policy
//     "blocked": ["<ver>", ...],        // preserved/overridable policy
//     "versions": {
//       "<ver>": {
//         "path":      "/tyde/v<ver>/",
//         "entry":     "/tyde/v<ver>/<entry>.js",
//         "integrity": "sha384-<base64>",          // entry JS digest
//         "artifacts": { "/tyde/v<ver>/<path>": "sha384-<base64>", ... }
//       }
//     }
//   }
//
// The loader (web/loader/manifest-policy.js + integrity.js) SRI-verifies the
// entry `integrity` PLUS every path in `artifacts` before the bundle runs, so
// this generator MUST enumerate EVERY executable artifact of the version — the
// entry `.js`, the `.wasm`, and any code-split chunks / wasm-bindgen snippets —
// not just the entry. A wasm or chunk left out is a wasm a tampered host could
// swap without tripping SRI.
//
// Merge is ADDITIVE: an existing manifest's other `versions`, `minSupported`,
// `blocked`, and `schemaVersion` are preserved; only `versions[<ver>]` is
// (re)written. Policy is changed only when explicitly passed (--min-supported /
// --blocked).
//
// No third-party deps — Node built-ins only.

import { createHash } from "node:crypto";
import { readFileSync, writeFileSync, existsSync, readdirSync, statSync } from "node:fs";
import { join, relative, sep, posix } from "node:path";

// --- arg parsing -----------------------------------------------------------

function parseArgs(argv) {
  const opts = { prefix: "/tyde" };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const next = () => {
      const v = argv[++i];
      if (v === undefined) fail(`missing value for ${a}`);
      return v;
    };
    switch (a) {
      case "--dist": opts.dist = next(); break;
      case "--version": opts.version = next(); break;
      case "--manifest": opts.manifest = next(); break;   // path to read+write
      case "--out": opts.out = next(); break;             // optional separate write target
      case "--prefix": opts.prefix = next(); break;       // default /tyde
      case "--entry": opts.entry = next(); break;         // optional entry override (relative to dist)
      case "--min-supported": opts.minSupported = next(); break;
      case "--blocked": opts.blocked = next(); break;     // CSV; replaces blocked list
      case "-h": case "--help": opts.help = true; break;
      default: fail(`unknown argument: ${a}`);
    }
  }
  return opts;
}

function fail(msg) {
  console.error(`generate-manifest: ${msg}`);
  process.exit(1);
}

function usage() {
  console.log(
    `Usage: node generate-manifest.mjs --dist <dir> --version <ver> --manifest <path> [options]

  --dist <dir>           Built Trunk output directory (contains entry .js + .wasm).
  --version <ver>        Release version (e.g. 0.8.19-beta.2). Validated like the host.
  --manifest <path>      Existing manifest to merge into AND write back (created if absent).
  --out <path>           Write merged manifest here instead of --manifest.
  --prefix <p>           Served prefix for URLs (default: /tyde).
  --entry <rel>          Entry JS path relative to dist (default: auto-detected).
  --min-supported <ver>  Set manifest.minSupported (default: preserve existing).
  --blocked <csv>        Set manifest.blocked to this comma list (default: preserve existing).
`,
  );
}

// --- version validation (mirror of web/loader/pairing.js) ------------------

const MAX_RELEASE_VERSION_LEN = 256;

function validateReleaseVersion(raw) {
  if (typeof raw !== "string") return null;
  if (raw.length > MAX_RELEASE_VERSION_LEN) return null;
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

// --- artifact discovery ----------------------------------------------------

const EXECUTABLE_EXT = new Set([".js", ".mjs", ".wasm"]);

function walk(dir, base = dir, acc = []) {
  for (const name of readdirSync(dir)) {
    // Skip AppleDouble sidecars and Trunk's transient stage dir.
    if (name.startsWith("._") || name === ".stage") continue;
    const full = join(dir, name);
    const st = statSync(full);
    if (st.isDirectory()) {
      walk(full, base, acc);
    } else if (st.isFile()) {
      acc.push(full);
    }
  }
  return acc;
}

function isExecutable(file) {
  const dot = file.lastIndexOf(".");
  return dot !== -1 && EXECUTABLE_EXT.has(file.slice(dot).toLowerCase());
}

// Returns the entry JS path RELATIVE to dist (posix separators), or null.
function detectEntry(dist, executables, override) {
  const rels = executables.map((f) => toPosixRel(dist, f));
  if (override) {
    const want = override.split(sep).join("/");
    if (!rels.includes(want)) {
      fail(`--entry ${override} not found among built artifacts`);
    }
    return want;
  }
  // 1. Parse Trunk's generated index.html for the bootstrap module import.
  const indexHtml = join(dist, "index.html");
  if (existsSync(indexHtml)) {
    const html = readFileSync(indexHtml, "utf8");
    const re = /import\s+[^'"]*['"]([^'"]+\.js)['"]/g;
    let m;
    let lastBase = null;
    while ((m = re.exec(html)) !== null) {
      lastBase = m[1].split("/").pop();
    }
    if (lastBase) {
      const hit = rels.find((r) => r.split("/").pop() === lastBase);
      if (hit) return hit;
    }
  }
  // 2. Heuristic: wasm-bindgen emits `<name>-<hash>_bg.wasm` paired with the
  //    `<name>-<hash>.js` glue that is the module entry.
  const bg = rels.find((r) => r.endsWith("_bg.wasm"));
  if (bg) {
    const candidate = bg.replace(/_bg\.wasm$/, ".js");
    if (rels.includes(candidate)) return candidate;
  }
  // 3. Last resort: a single top-level (non-snippet) .js file.
  const topJs = rels.filter((r) => r.endsWith(".js") && !r.includes("/"));
  if (topJs.length === 1) return topJs[0];
  return null;
}

function toPosixRel(base, file) {
  return relative(base, file).split(sep).join(posix.sep);
}

function sriFor(file) {
  const buf = readFileSync(file);
  return "sha384-" + createHash("sha384").update(buf).digest("base64");
}

// --- main ------------------------------------------------------------------

const opts = parseArgs(process.argv.slice(2));
if (opts.help) {
  usage();
  process.exit(0);
}
if (!opts.dist) fail("--dist is required");
if (!opts.version) fail("--version is required");
if (!opts.manifest && !opts.out) fail("--manifest (or --out) is required");

const version = validateReleaseVersion(opts.version);
if (!version) fail(`invalid release version: ${JSON.stringify(opts.version)}`);

if (!existsSync(opts.dist) || !statSync(opts.dist).isDirectory()) {
  fail(`dist directory not found: ${opts.dist}`);
}

const prefix = opts.prefix.replace(/\/+$/, ""); // strip trailing slash
if (prefix !== "/tyde") {
  // The loader hard-rejects any path not under /tyde/; warn loudly if someone
  // points this elsewhere so it never silently produces an unbootable manifest.
  console.error(
    `generate-manifest: WARNING prefix is ${prefix}, not /tyde — the loader only boots /tyde/ paths.`,
  );
}
const base = `${prefix}/v${version}/`;

const allFiles = walk(opts.dist);
const executables = allFiles.filter(isExecutable).sort();
if (executables.length === 0) {
  fail(`no executable artifacts (.js/.mjs/.wasm) found under ${opts.dist} — was the bundle built?`);
}

const entryRel = detectEntry(opts.dist, executables, opts.entry);
if (!entryRel) {
  fail(
    "could not determine the entry .js — pass --entry <relative-path>. " +
      `candidates: ${executables.map((f) => toPosixRel(opts.dist, f)).join(", ")}`,
  );
}

// Build the artifact map. Entry goes in `integrity`; EVERY other executable
// (wasm + chunks + snippets) goes in `artifacts`.
const entryFile = join(opts.dist, entryRel.split("/").join(sep));
const entryUrl = base + entryRel;
const entryIntegrity = sriFor(entryFile);

const artifacts = {};
let wasmCount = 0;
for (const file of executables) {
  const rel = toPosixRel(opts.dist, file);
  if (rel === entryRel) continue;
  const url = base + rel;
  artifacts[url] = sriFor(file);
  if (rel.endsWith(".wasm")) wasmCount++;
}

if (wasmCount === 0) {
  // A Trunk WASM bundle without a .wasm artifact means discovery missed it; the
  // loader would then boot an un-pinned wasm. Fail loudly.
  fail("no .wasm artifact discovered — refusing to emit a manifest that leaves wasm un-pinned");
}

// Load + merge existing manifest (additive).
const readPath = opts.manifest;
let manifest = { schemaVersion: 1, versions: {} };
if (readPath && existsSync(readPath)) {
  try {
    const parsed = JSON.parse(readFileSync(readPath, "utf8"));
    if (parsed && typeof parsed === "object") manifest = parsed;
  } catch (err) {
    fail(`existing manifest is not valid JSON (${readPath}): ${err.message}`);
  }
}
if (typeof manifest.schemaVersion !== "number") manifest.schemaVersion = 1;
if (!manifest.versions || typeof manifest.versions !== "object" || Array.isArray(manifest.versions)) {
  manifest.versions = {};
}

// Optional policy overrides (otherwise preserved verbatim).
if (opts.minSupported !== undefined) {
  const min = validateReleaseVersion(opts.minSupported);
  if (!min) fail(`invalid --min-supported: ${JSON.stringify(opts.minSupported)}`);
  manifest.minSupported = min;
}
if (opts.blocked !== undefined) {
  const list = opts.blocked
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
  for (const b of list) {
    if (!validateReleaseVersion(b)) fail(`invalid --blocked entry: ${JSON.stringify(b)}`);
  }
  manifest.blocked = list;
}

// (Re)write this version's record.
manifest.versions[version] = {
  path: base,
  entry: entryUrl,
  integrity: entryIntegrity,
  artifacts,
};

const json = JSON.stringify(manifest, null, 2) + "\n";
const writePath = opts.out || opts.manifest;
writeFileSync(writePath, json);

// Summary to stderr so stdout stays clean if piped.
const totalArtifacts = 1 + Object.keys(artifacts).length;
console.error(`generate-manifest: version ${version}`);
console.error(`  entry:     ${entryUrl}`);
console.error(`  integrity: ${entryIntegrity}`);
console.error(`  artifacts: ${totalArtifacts} executable (1 entry + ${Object.keys(artifacts).length} other, ${wasmCount} wasm)`);
for (const [url, sri] of Object.entries(artifacts)) {
  console.error(`    ${url}  ${sri}`);
}
console.error(`  written:   ${writePath}`);
console.error(`  versions now in manifest: ${Object.keys(manifest.versions).sort().join(", ")}`);
if (manifest.minSupported) console.error(`  minSupported: ${manifest.minSupported}`);
if (manifest.blocked) console.error(`  blocked: ${JSON.stringify(manifest.blocked)}`);
