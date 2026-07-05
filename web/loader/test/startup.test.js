import { test } from "node:test";
import assert from "node:assert/strict";

import {
  decodeStoredPairedHostsState,
  resolveStartupTarget,
} from "../loader.js";
import { resolveBootTarget } from "../manifest-policy.js";

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

const SELF_HEAL_MANIFEST = {
  schemaVersion: 1,
  minSupported: "0.8.19-beta.16",
  blocked: [],
  versions: {
    "0.8.19-beta.15": entry("0.8.19-beta.15"),
    "0.8.19-beta.16": entry("0.8.19-beta.16"),
    "0.8.19-beta.17": entry("0.8.19-beta.17"),
  },
};

const BETA16_LATEST_MANIFEST = {
  schemaVersion: 1,
  minSupported: "0.8.19-beta.16",
  blocked: [],
  versions: {
    "0.8.19-beta.15": entry("0.8.19-beta.15"),
    "0.8.19-beta.16": entry("0.8.19-beta.16"),
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

test("startup target skips remembered beta15 below beta16 floor with paired hosts", () => {
  const target = resolveStartupTarget(BETA16_LATEST_MANIFEST, "0.8.19-beta.15", true);
  assert.equal(target.ok, true);
  assert.equal(target.version, "0.8.19-beta.16");
  assert.equal(target.source, "latest");
});

test("startup target keeps remembered beta16 when paired hosts exist", () => {
  const target = resolveStartupTarget(SELF_HEAL_MANIFEST, "0.8.19-beta.16", true);
  assert.equal(target.ok, true);
  assert.equal(target.version, "0.8.19-beta.16");
  assert.equal(target.source, "remembered");
});

test("startup target still prefers supported remembered versions over latest", () => {
  const target = resolveStartupTarget(SELF_HEAL_MANIFEST, "0.8.19-beta.16", null);
  assert.equal(target.ok, true);
  assert.equal(target.version, "0.8.19-beta.16");
  assert.equal(target.source, "remembered");
});

test("startup target uses latest beta17 when beta16 is remembered but no hosts are paired", () => {
  const target = resolveStartupTarget(SELF_HEAL_MANIFEST, "0.8.19-beta.16", false);
  assert.equal(target.ok, true);
  assert.equal(target.version, "0.8.19-beta.17");
  assert.equal(target.source, "latest");
});

test("version-only repair target below beta16 floor is rejected", () => {
  const target = resolveBootTarget("0.8.19-beta.15", BETA16_LATEST_MANIFEST);
  assert.equal(target.ok, false);
  assert.equal(target.reason, "below-min-supported");
});
