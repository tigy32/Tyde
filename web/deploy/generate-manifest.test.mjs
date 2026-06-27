import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";

const SCRIPT = new URL("./generate-manifest.mjs", import.meta.url).pathname;

function tempDir() {
  return mkdtempSync(join(tmpdir(), "tyde-manifest-test-"));
}

function writeDist(root) {
  const dist = join(root, "dist");
  mkdirSync(dist);
  writeFileSync(
    join(dist, "index.html"),
    `<script type="module">import init from '/tyde/v0.8.19-beta.9/mobile-frontend-test.js';</script>`,
  );
  writeFileSync(join(dist, "mobile-frontend-test.js"), "export default function init() {}\n");
  writeFileSync(join(dist, "mobile-frontend-test_bg.wasm"), "wasm bytes\n");
  return dist;
}

test("generator stamps protocolVersion from Rust protocol source", () => {
  const root = tempDir();
  const dist = writeDist(root);
  const manifest = join(root, "manifest.json");
  const protocolSource = join(root, "types.rs");
  writeFileSync(protocolSource, "pub const PROTOCOL_VERSION: u32 = 42;\n");
  writeFileSync(
    manifest,
    JSON.stringify(
      {
        schemaVersion: 1,
        minSupported: "0.8.19-beta.1",
        versions: {
          "0.8.19-beta.8": {
            path: "/tyde/v0.8.19-beta.8/",
            entry: "/tyde/v0.8.19-beta.8/old.js",
            integrity: "sha384-" + "A".repeat(64),
            protocolVersion: 41,
            artifacts: {},
          },
        },
      },
      null,
      2,
    ),
  );

  const result = spawnSync(
    process.execPath,
    [
      SCRIPT,
      "--dist",
      dist,
      "--version",
      "0.8.19-beta.9",
      "--manifest",
      manifest,
      "--protocol-source",
      protocolSource,
    ],
    { encoding: "utf8" },
  );

  assert.equal(result.status, 0, result.stderr);
  const generated = JSON.parse(readFileSync(manifest, "utf8"));
  assert.equal(generated.minSupported, "0.8.19-beta.8");
  assert.ok(generated.versions["0.8.19-beta.8"], "preserves existing entries");
  const entry = generated.versions["0.8.19-beta.9"];
  assert.equal(entry.protocolVersion, 42);
  assert.equal(entry.path, "/tyde/v0.8.19-beta.9/");
  assert.equal(entry.entry, "/tyde/v0.8.19-beta.9/mobile-frontend-test.js");
  assert.ok(entry.integrity.startsWith("sha384-"));
  assert.ok(entry.artifacts["/tyde/v0.8.19-beta.9/mobile-frontend-test_bg.wasm"]);
});

test("generator raises minSupported past entries without protocolVersion", () => {
  const root = tempDir();
  const dist = writeDist(root);
  const manifest = join(root, "manifest.json");
  const protocolSource = join(root, "types.rs");
  writeFileSync(protocolSource, "pub const PROTOCOL_VERSION: u32 = 42;\n");
  writeFileSync(
    manifest,
    JSON.stringify({
      schemaVersion: 1,
      minSupported: "0.8.19-beta.1",
      versions: {
        "0.8.19-beta.8": {
          path: "/tyde/v0.8.19-beta.8/",
          entry: "/tyde/v0.8.19-beta.8/old.js",
          integrity: "sha384-" + "A".repeat(64),
          artifacts: {
            "/tyde/v0.8.19-beta.8/old_bg.wasm": "sha384-" + "B".repeat(64),
          },
        },
      },
    }),
  );

  const result = spawnSync(
    process.execPath,
    [
      SCRIPT,
      "--dist",
      dist,
      "--version",
      "0.8.19-beta.9",
      "--manifest",
      manifest,
      "--protocol-source",
      protocolSource,
    ],
    { encoding: "utf8" },
  );

  assert.equal(result.status, 0, result.stderr);
  const generated = JSON.parse(readFileSync(manifest, "utf8"));
  assert.equal(generated.minSupported, "0.8.19-beta.9");
});

test("generator fails when protocol source lacks PROTOCOL_VERSION", () => {
  const root = tempDir();
  const dist = writeDist(root);
  const manifest = join(root, "manifest.json");
  const protocolSource = join(root, "types.rs");
  writeFileSync(protocolSource, "pub const TYDE_VERSION: u32 = 1;\n");

  const result = spawnSync(
    process.execPath,
    [
      SCRIPT,
      "--dist",
      dist,
      "--version",
      "0.8.19-beta.9",
      "--manifest",
      manifest,
      "--protocol-source",
      protocolSource,
    ],
    { encoding: "utf8" },
  );

  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /PROTOCOL_VERSION/);
});

test("generator fails closed on malformed existing manifest", () => {
  const root = tempDir();
  const dist = writeDist(root);
  const manifest = join(root, "manifest.json");
  const protocolSource = join(root, "types.rs");
  writeFileSync(protocolSource, "pub const PROTOCOL_VERSION: u32 = 42;\n");
  writeFileSync(manifest, JSON.stringify({ versions: [] }));

  const result = spawnSync(
    process.execPath,
    [
      SCRIPT,
      "--dist",
      dist,
      "--version",
      "0.8.19-beta.9",
      "--manifest",
      manifest,
      "--protocol-source",
      protocolSource,
    ],
    { encoding: "utf8" },
  );

  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /manifest\.versions/);
});
