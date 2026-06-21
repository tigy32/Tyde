// Tests for the bundle stylesheet discovery/validation (styles.js). The booted
// versioned app needs its own CSS injected from its index.html; these tests pin
// the extraction and the security confinement (same-origin, within the version
// directory, SRI preserved).

import { test } from "node:test";
import assert from "node:assert/strict";

import { extractStylesheetLinks, resolveBundleStylesheets } from "../styles.js";

const ORIGIN = "https://tycode.dev";
const VERSION_PATH = "/tyde/v0.8.19-beta.2/";
const BASE = ORIGIN + VERSION_PATH + "index.html";

// A trimmed copy of the real deployed bundle index.html head.
const REAL_INDEX = `<!DOCTYPE html><html><head>
  <meta charset="utf-8" />
  <link rel="stylesheet" href="/tyde/v0.8.19-beta.2/styles-39775950ec5da72.css" integrity="sha384-bd+fPMMs6wPsjd9BsDBe+9Jq++NOCfsINqsIAd2aCqzJgoQl3nnIg5VxxtwSHYUJ"/>
  <script type="module">import init from '/tyde/v0.8.19-beta.2/mobile-frontend-fbdcf453e007f345.js';</script>
  <style>html{background:#0b0f14}</style>
</head><body></body></html>`;

test("extractStylesheetLinks pulls href + integrity from the bundle index", () => {
  const links = extractStylesheetLinks(REAL_INDEX);
  assert.equal(links.length, 1);
  assert.equal(links[0].href, "/tyde/v0.8.19-beta.2/styles-39775950ec5da72.css");
  assert.match(links[0].integrity, /^sha384-/);
});

test("extractStylesheetLinks ignores non-stylesheet links and handles rel token lists", () => {
  const html = `
    <link rel="icon" href="/tyde/v0.8.19-beta.2/icon.svg" />
    <link rel="modulepreload" href="/tyde/v0.8.19-beta.2/x.js" />
    <link rel="preload stylesheet" href="/tyde/v0.8.19-beta.2/late.css" />`;
  const links = extractStylesheetLinks(html);
  assert.equal(links.length, 1);
  assert.equal(links[0].href, "/tyde/v0.8.19-beta.2/late.css");
});

test("resolveBundleStylesheets returns same-origin in-version links with SRI", () => {
  const sheets = resolveBundleStylesheets(REAL_INDEX, {
    baseHref: BASE,
    versionPath: VERSION_PATH,
    origin: ORIGIN,
  });
  assert.equal(sheets.length, 1);
  assert.equal(sheets[0].href, "/tyde/v0.8.19-beta.2/styles-39775950ec5da72.css");
  assert.match(sheets[0].integrity, /^sha384-/);
});

test("resolveBundleStylesheets drops off-origin, out-of-version, and traversal hrefs", () => {
  const evil = `
    <link rel="stylesheet" href="https://evil.example/x.css" />
    <link rel="stylesheet" href="//evil.example/y.css" />
    <link rel="stylesheet" href="/tyde/v9.9.9/other.css" />
    <link rel="stylesheet" href="/tyde/v0.8.19-beta.2/../../evil.css" />
    <link rel="stylesheet" href="/tyde/v0.8.19-beta.2/%2e%2e/evil.css" />`;
  const sheets = resolveBundleStylesheets(evil, {
    baseHref: BASE,
    versionPath: VERSION_PATH,
    origin: ORIGIN,
  });
  assert.deepEqual(sheets, []);
});

test("resolveBundleStylesheets keeps a valid in-version link but strips malformed SRI", () => {
  const html = `<link rel="stylesheet" href="/tyde/v0.8.19-beta.2/app.css" integrity="md5-nope" />`;
  const sheets = resolveBundleStylesheets(html, {
    baseHref: BASE,
    versionPath: VERSION_PATH,
    origin: ORIGIN,
  });
  assert.equal(sheets.length, 1);
  assert.equal(sheets[0].href, "/tyde/v0.8.19-beta.2/app.css");
  assert.equal(sheets[0].integrity, null);
});

test("resolveBundleStylesheets resolves relative hrefs against the index URL", () => {
  const html = `<link rel="stylesheet" href="styles-abc.css" />`;
  const sheets = resolveBundleStylesheets(html, {
    baseHref: BASE,
    versionPath: VERSION_PATH,
    origin: ORIGIN,
  });
  assert.equal(sheets.length, 1);
  assert.equal(sheets[0].href, "/tyde/v0.8.19-beta.2/styles-abc.css");
});

test("resolveBundleStylesheets refuses a non-/tyde version path", () => {
  const html = `<link rel="stylesheet" href="/evil/app.css" />`;
  assert.deepEqual(
    resolveBundleStylesheets(html, { baseHref: ORIGIN + "/", versionPath: "/evil/", origin: ORIGIN }),
    [],
  );
});
