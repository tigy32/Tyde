// Structural guard for the loader/app boot handoff. The loader chrome lives in
// `#loader-shell`; the booted WASM app mounts into a SEPARATE, initially-empty
// `#app-root`. If these two ever collapse back into one container, the app
// mounts hidden behind the loader spinner — the exact bug this split fixes.
//
// These are text assertions on index.html (no DOM in `node --test`); they check
// the contract the runtime handoff in loader.js depends on. The MutationObserver
// handoff itself needs a browser and is intentionally not unit-tested here.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const html = readFileSync(
  fileURLToPath(new URL("../index.html", import.meta.url)),
  "utf8",
);
const loaderJs = readFileSync(
  fileURLToPath(new URL("../loader.js", import.meta.url)),
  "utf8",
);
const loaderCss = readFileSync(
  fileURLToPath(new URL("../loader.css", import.meta.url)),
  "utf8",
);

test("index.html has BOTH a loader shell and a distinct app mount target", () => {
  assert.match(html, /id="loader-shell"/, "expected the loader chrome container");
  assert.match(html, /id="app-root"/, "expected the app mount target");
  // They must be different elements — the whole point of the fix.
  assert.notEqual(
    html.indexOf('id="loader-shell"'),
    html.indexOf('id="app-root"'),
  );
  // Exactly one of each.
  assert.equal((html.match(/id="loader-shell"/g) || []).length, 1);
  assert.equal((html.match(/id="app-root"/g) || []).length, 1);
});

test("the four loader views live inside #loader-shell, not #app-root", () => {
  const shellStart = html.indexOf('id="loader-shell"');
  const shellEnd = html.indexOf("</main>");
  assert.ok(shellStart >= 0 && shellEnd > shellStart, "loader-shell <main> present");
  const shell = html.slice(shellStart, shellEnd);
  for (const view of ["view-loading", "view-pair", "view-booting", "view-error"]) {
    assert.match(shell, new RegExp(`id="${view}"`), `${view} should be in the shell`);
  }
});

test("#app-root is an empty container the app can mount into", () => {
  // Match the app-root div and assert it carries no child markup of its own.
  const m = html.match(/<div id="app-root">([\s\S]*?)<\/div>/);
  assert.ok(m, "expected a <div id=\"app-root\"></div>");
  assert.equal(m[1].trim(), "", "#app-root must start empty");
});

test("loader.js performs the handoff: observe #app-root, hide #loader-shell", () => {
  assert.match(loaderJs, /MutationObserver/);
  assert.match(loaderJs, /getElementById\("app-root"\)/);
  assert.match(loaderJs, /hideLoaderShell/);
  // The error path must re-show the shell so the error view stays visible.
  assert.match(loaderJs, /showLoaderShell/);
});

test("loader.js boots Trunk-style: dynamic import + init({module_or_path})", () => {
  // The entry module only EXPORTS init; a bare <script src> would load it but
  // never instantiate the wasm. The loader must import() the entry and call its
  // init() with the explicit hashed wasm path.
  assert.match(loaderJs, /await import\(/, "expected a dynamic import of the entry module");
  assert.match(loaderJs, /module_or_path:/, "expected init() to receive an explicit wasm path");
  assert.match(loaderJs, /selectBootUrls/, "expected the entry/wasm URLs to be resolved from the verified target");
  // The old, broken boot (a <script type=module src> that never calls init)
  // must be gone.
  assert.doesNotMatch(loaderJs, /script\.src\s*=\s*target\.entry/, "the <script src> entry injection must be removed");
});

test("loader.css hides #loader-shell when the hidden attribute is set", () => {
  assert.match(loaderCss, /#loader-shell\[hidden\]\s*\{\s*display:\s*none/);
});
