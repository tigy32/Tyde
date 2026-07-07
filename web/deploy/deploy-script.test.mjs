import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const SCRIPT = readFileSync(new URL("./deploy.sh", import.meta.url), "utf8");

test("real deploys always start from the live manifest", () => {
  assert.match(
    SCRIPT,
    /if \[ "\$\{CONFIRM\}" -eq 1 \]; then\n  LIVE_MANIFEST_BASE=1\nfi/,
  );
  assert.match(SCRIPT, /aws s3 cp "\$\{S3_LOADER\}manifest\.json" "\$\{LIVE_MANIFEST\}"/);
  assert.match(SCRIPT, /aws s3 cp "\$\{S3_LOADER\}manifest\.json" "\$\{LATEST_MANIFEST\}"/);
});

test("loader shell upload rewrites metadata for unchanged files", () => {
  assert.doesNotMatch(SCRIPT, /aws s3 sync "\$\{LOADER_DIR\}\/" "\$\{S3_LOADER\}"/);
  assert.match(SCRIPT, /aws s3 cp "\$\{LOADER_DIR\}\/" "\$\{S3_LOADER\}"/);
  assert.match(SCRIPT, /--recursive/);
  assert.match(SCRIPT, /--cache-control "\$\{LOADER_SHELL_CACHE_CONTROL\}"/);
});

test("loader shell and manifest cache headers are validated", () => {
  assert.match(SCRIPT, /"index\.html"/);
  assert.match(SCRIPT, /"sw\.js"/);
  assert.match(SCRIPT, /"mobile-service-config\.js"/);
  assert.match(SCRIPT, /"loader\.js"/);
  assert.match(SCRIPT, /--key "\$\{PREFIX\}\/\$\{shell_key\}"/);
  assert.match(SCRIPT, /--query CacheControl/);
  assert.match(SCRIPT, /\[ "\$\{cache_control\}" = "\$\{LOADER_SHELL_CACHE_CONTROL\}" \]/);
  assert.match(SCRIPT, /--key "\$\{PREFIX\}\/manifest\.json"/);
  assert.match(
    SCRIPT,
    /\[ "\$\{manifest_cache_control\}" = "\$\{MANIFEST_CACHE_CONTROL\}" \]/,
  );
});

test("mobile service config deploys as loader shell, not manifest authority", () => {
  assert.match(SCRIPT, /"mobile-service-config\.js"/);
  assert.match(SCRIPT, /aws s3 cp "\$\{LOADER_DIR\}\/" "\$\{S3_LOADER\}"/);
  assert.match(SCRIPT, /--exclude 'manifest\.json'/);
  assert.doesNotMatch(SCRIPT, /mobile-service-config\.js.*MANIFEST_CACHE_CONTROL/);
});
