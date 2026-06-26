import { test } from "node:test";
import assert from "node:assert/strict";

import {
  decodeStoredPairedHostsState,
  resolveStartupTarget,
} from "../loader.js";

const integrity = "sha384-" + "A".repeat(64);
const wasmIntegrity = "sha384-" + "B".repeat(64);

function entry(version) {
  return {
    path: `/tyde/v${version}/`,
    entry: `/tyde/v${version}/app.js`,
    integrity,
    artifacts: {
      [`/tyde/v${version}/app_bg.wasm`]: wasmIntegrity,
    },
  };
}

const MANIFEST = {
  schemaVersion: 1,
  minSupported: "0.8.19-beta.1",
  blocked: [],
  versions: {
    "0.8.19-beta.4": entry("0.8.19-beta.4"),
    "0.8.19-beta.8": entry("0.8.19-beta.8"),
  },
};

test("decodeStoredPairedHostsState identifies empty and populated stores", () => {
  assert.equal(decodeStoredPairedHostsState(null), false);
  assert.equal(decodeStoredPairedHostsState(undefined), false);
  assert.equal(decodeStoredPairedHostsState("[]"), false);
  assert.equal(decodeStoredPairedHostsState('[{"localHostId":"host-one"}]'), true);
  assert.equal(decodeStoredPairedHostsState("not-json"), null);
  assert.equal(decodeStoredPairedHostsState("{}"), null);
});

test("startup target uses remembered version when paired hosts exist", () => {
  const target = resolveStartupTarget(MANIFEST, "0.8.19-beta.4", true);
  assert.equal(target.ok, true);
  assert.equal(target.version, "0.8.19-beta.4");
  assert.equal(target.source, "remembered");
});

test("startup target preserves remembered version when host storage is unknown", () => {
  const target = resolveStartupTarget(MANIFEST, "0.8.19-beta.4", null);
  assert.equal(target.ok, true);
  assert.equal(target.version, "0.8.19-beta.4");
  assert.equal(target.source, "remembered");
});

test("startup target ignores stale remembered version when no hosts are paired", () => {
  const target = resolveStartupTarget(MANIFEST, "0.8.19-beta.4", false);
  assert.equal(target.ok, true);
  assert.equal(target.version, "0.8.19-beta.8");
  assert.equal(target.source, "latest");
});

test("startup target boots latest when no remembered version exists", () => {
  const target = resolveStartupTarget(MANIFEST, null, false);
  assert.equal(target.ok, true);
  assert.equal(target.version, "0.8.19-beta.8");
  assert.equal(target.source, "latest");
});
