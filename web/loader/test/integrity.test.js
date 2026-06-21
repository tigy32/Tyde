import { test } from "node:test";
import assert from "node:assert/strict";

import { verifyArtifacts } from "../integrity.js";

// Computes a real SRI string for the given bytes so fixtures are self-consistent
// (no hand-copied digests).
async function sri(bytes, algo = "SHA-384", label = "sha384") {
  const digest = await crypto.subtle.digest(algo, bytes);
  return `${label}-${Buffer.from(new Uint8Array(digest)).toString("base64")}`;
}

function fakeResponse(bytes) {
  return {
    ok: true,
    clone() {
      return fakeResponse(bytes);
    },
    async arrayBuffer() {
      return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
    },
  };
}

// A fetch stub backed by a url -> bytes map. Unknown urls 404.
function fakeFetch(map) {
  return async (url) => {
    if (url in map) return fakeResponse(map[url]);
    return {
      ok: false,
      clone() {
        return this;
      },
      async arrayBuffer() {
        return new ArrayBuffer(0);
      },
    };
  };
}

function fakeCache() {
  const store = {};
  return {
    store,
    async put(url, res) {
      store[url] = res;
    },
    async delete(url) {
      delete store[url];
    },
  };
}

const enc = (s) => new TextEncoder().encode(s);

test("verifyArtifacts passes when every artifact hash matches", async () => {
  const js = enc("export const x = 1;");
  const wasm = enc("\0asm fake wasm bytes");
  const artifacts = [
    { url: "/tyde/v1.2.3/app.js", integrity: await sri(js) },
    { url: "/tyde/v1.2.3/app_bg.wasm", integrity: await sri(wasm) },
  ];
  const cache = fakeCache();
  const result = await verifyArtifacts(artifacts, {
    fetchImpl: fakeFetch({
      "/tyde/v1.2.3/app.js": js,
      "/tyde/v1.2.3/app_bg.wasm": wasm,
    }),
    cache,
  });
  assert.deepEqual(result, { ok: true, failures: [] });
  // Both verified artifacts were cached.
  assert.ok(cache.store["/tyde/v1.2.3/app.js"]);
  assert.ok(cache.store["/tyde/v1.2.3/app_bg.wasm"]);
});

test("verifyArtifacts rejects a TAMPERED wasm even when the entry JS is intact", async () => {
  const js = enc("export const x = 1;");
  const wasmReal = enc("\0asm legitimate");
  const wasmTampered = enc("\0asm MALICIOUS payload");
  // Manifest pins the REAL wasm hash; the server serves tampered bytes.
  const artifacts = [
    { url: "/tyde/v1.2.3/app.js", integrity: await sri(js) },
    { url: "/tyde/v1.2.3/app_bg.wasm", integrity: await sri(wasmReal) },
  ];
  const cache = fakeCache();
  const result = await verifyArtifacts(artifacts, {
    fetchImpl: fakeFetch({
      "/tyde/v1.2.3/app.js": js,
      "/tyde/v1.2.3/app_bg.wasm": wasmTampered,
    }),
    cache,
  });
  assert.equal(result.ok, false);
  assert.deepEqual(result.failures, ["/tyde/v1.2.3/app_bg.wasm"]);
  // The intact entry was cached; the tampered wasm was NOT (no cache-before-verify).
  assert.ok(cache.store["/tyde/v1.2.3/app.js"]);
  assert.equal(cache.store["/tyde/v1.2.3/app_bg.wasm"], undefined);
});

test("verifyArtifacts rejects a tampered code-split chunk", async () => {
  const js = enc("entry");
  const chunkReal = enc("chunk-real");
  const artifacts = [
    { url: "/tyde/v1.2.3/app.js", integrity: await sri(js) },
    { url: "/tyde/v1.2.3/chunk-abc.js", integrity: await sri(chunkReal) },
  ];
  const result = await verifyArtifacts(artifacts, {
    fetchImpl: fakeFetch({
      "/tyde/v1.2.3/app.js": js,
      "/tyde/v1.2.3/chunk-abc.js": enc("chunk-TAMPERED"),
    }),
  });
  assert.equal(result.ok, false);
  assert.deepEqual(result.failures, ["/tyde/v1.2.3/chunk-abc.js"]);
});

test("verifyArtifacts fails an artifact that 404s", async () => {
  const js = enc("entry");
  const artifacts = [
    { url: "/tyde/v1.2.3/app.js", integrity: await sri(js) },
    { url: "/tyde/v1.2.3/missing.wasm", integrity: await sri(enc("x")) },
  ];
  const result = await verifyArtifacts(artifacts, {
    fetchImpl: fakeFetch({ "/tyde/v1.2.3/app.js": js }),
  });
  assert.equal(result.ok, false);
  assert.deepEqual(result.failures, ["/tyde/v1.2.3/missing.wasm"]);
});

test("verifyArtifacts fails closed when crypto/fetch are unavailable", async () => {
  const artifacts = [{ url: "/tyde/v1.2.3/app.js", integrity: "sha384-x" }];
  const result = await verifyArtifacts(artifacts, { fetchImpl: null, subtle: null });
  assert.equal(result.ok, false);
  assert.deepEqual(result.failures, ["/tyde/v1.2.3/app.js"]);
});
