// Stylesheet discovery for the booted versioned bundle. Pure module (no DOM /
// no network) so it can be unit-tested under `node --test`.
//
// WHY THIS EXISTS: the loader boots the immutable versioned bundle by injecting
// the bundle's entry <script>. But the bundle's OWN index.html also carries a
// `<link rel="stylesheet" href="/tyde/v<ver>/styles-<hash>.css" integrity="…">`
// that the loader was NOT injecting — so the mounted Leptos app rendered with
// zero CSS. This module extracts those stylesheet links from the version's
// index.html so the loader can inject them alongside the entry script.
//
// SECURITY: the bundle's index.html is same-origin and immutable, but we still
// treat its hrefs as untrusted strings. Every link is confined to the version's
// own `/tyde/v<ver>/…` directory (same origin, no traversal) and its declared
// Subresource Integrity is preserved (and only kept if syntactically valid).
// Anything that escapes the version path — protocol-relative, off-origin,
// traversal, percent-encoded traversal — is dropped, not injected.

// Same SRI grammar the manifest policy enforces: a supported hash name plus a
// base64 digest.
const INTEGRITY_RE = /^sha(256|384|512)-[A-Za-z0-9+/]+={0,2}$/;

function isValidIntegrity(value) {
  return typeof value === "string" && INTEGRITY_RE.test(value);
}

// Reads a single (possibly quoted) HTML attribute value out of a tag string.
// Returns null when the attribute is absent. Tolerates double-quoted,
// single-quoted, and unquoted values.
function readAttr(tag, name) {
  const re = new RegExp(
    `\\b${name}\\s*=\\s*("([^"]*)"|'([^']*)'|([^\\s"'>]+))`,
    "i",
  );
  const m = re.exec(tag);
  if (!m) return null;
  const value = m[2] !== undefined ? m[2] : m[3] !== undefined ? m[3] : m[4];
  return value === undefined ? null : value;
}

// Extracts every `<link rel="stylesheet">` from an HTML string as
// `{ href, integrity, crossorigin }` (integrity/crossorigin null when absent).
// Regex-based on purpose: this must run unchanged in Node tests (no DOMParser)
// and in the browser. `rel` is matched token-wise so `rel="stylesheet preload"`
// still counts.
export function extractStylesheetLinks(html) {
  if (typeof html !== "string") return [];
  const out = [];
  const tagRe = /<link\b[^>]*>/gi;
  let match;
  while ((match = tagRe.exec(html)) !== null) {
    const tag = match[0];
    const rel = readAttr(tag, "rel");
    if (!rel) continue;
    if (!rel.toLowerCase().split(/\s+/).includes("stylesheet")) continue;
    const href = readAttr(tag, "href");
    if (!href) continue;
    out.push({
      href,
      integrity: readAttr(tag, "integrity"),
      crossorigin: readAttr(tag, "crossorigin"),
    });
  }
  return out;
}

// Resolves and validates the stylesheet links of a version's index.html against
// that version's own directory. Returns the subset that is safe to inject as
// `{ href, integrity, crossorigin }`, where `href` is the resolved same-origin
// pathname (+search) and `integrity` is preserved only when syntactically
// valid. `versionPath` is the manifest-validated `/tyde/v<ver>/` prefix;
// `baseHref` is the absolute URL of the index.html the links came from; `origin`
// is the expected origin (the loader's own). Any link that does not resolve
// within `origin` + `versionPath` is dropped.
export function resolveBundleStylesheets(html, { baseHref, versionPath, origin } = {}) {
  if (typeof versionPath !== "string" || !versionPath.startsWith("/tyde/")) {
    return [];
  }
  const safe = [];
  for (const link of extractStylesheetLinks(html)) {
    let resolved;
    try {
      resolved = new URL(link.href, baseHref);
    } catch {
      continue; // unparseable href
    }
    if (origin && resolved.origin !== origin) continue; // off-origin
    if (!resolved.pathname.startsWith(versionPath)) continue; // outside version dir
    if (resolved.pathname.includes("..")) continue; // traversal
    if (/%2e|%2f|%5c/i.test(resolved.pathname)) continue; // encoded traversal
    safe.push({
      href: resolved.pathname + resolved.search,
      integrity: isValidIntegrity(link.integrity) ? link.integrity : null,
      crossorigin: link.crossorigin,
    });
  }
  return safe;
}
