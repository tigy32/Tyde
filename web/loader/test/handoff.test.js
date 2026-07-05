// #3 — the loader must hand the full raw pairing URI to the booted WASM app so
// first-time pairing can complete. It stashes the URI in sessionStorage under
// PAIR_URI_KEY (consumed by mobile-frontend's bridge::web::take_pending_pairing_uri).
//
// We stub the minimal browser globals and call the exported handlePairingUri
// directly. `document` is intentionally left undefined so loader.js does NOT
// auto-run init() on import.

import { test } from "node:test";
import assert from "node:assert/strict";

// Install stubs BEFORE importing loader.js.
const sessionStore = {};
const localStore = {};
globalThis.sessionStorage = {
  getItem: (k) => (k in sessionStore ? sessionStore[k] : null),
  setItem: (k, v) => {
    sessionStore[k] = String(v);
  },
  removeItem: (k) => {
    delete sessionStore[k];
  },
};
globalThis.localStorage = {
  getItem: (k) => (k in localStore ? localStore[k] : null),
  setItem: (k, v) => {
    localStore[k] = String(v);
  },
  removeItem: (k) => {
    delete localStore[k];
  },
};
// Artifact fetches return bytes that will NOT match the placeholder manifest
// hashes, so verification fails and boot aborts — AFTER the URI is stashed,
// which is exactly the behavior under test.
globalThis.fetch = async () => ({
  ok: true,
  clone() {
    return this;
  },
  async arrayBuffer() {
    return new TextEncoder().encode("not-the-real-bundle").buffer;
  },
});

const {
  handlePairingUri,
  PAIR_URI_KEY,
  REPAIR_URI_KEY,
  REPAIR_ATTEMPTS_KEY,
  MAX_REPAIR_ATTEMPTS,
  STORAGE_KEY,
  PENDING_FRAGMENT_KEY,
  checkProtocolCompatibility,
  takeRepairUri,
  registerRepairAttempt,
  clearRepairAttempts,
  onRepairNeeded,
  onRepairVersion,
  takeRepairVersion,
  REPAIR_VERSION_KEY,
  resolvePairingFragment,
} = await import("../loader.js");
const { REAL_WITH_PRERELEASE, EXAMPLE_MANIFEST, makePairingUri } = await import(
  "./fixtures.js"
);

// EXAMPLE_MANIFEST stamps protocolVersion 13 on 0.8.19-beta.2; REAL_WITH_PRERELEASE
// is a protocol-13 QR, so the happy path matches. These helpers derive drift
// manifests from it.
function manifestWithBetaProtocol(pv) {
  const m = structuredClone(EXAMPLE_MANIFEST);
  if (pv === undefined) delete m.versions["0.8.19-beta.2"].protocolVersion;
  else m.versions["0.8.19-beta.2"].protocolVersion = pv;
  return m;
}

test("handlePairingUri stashes the raw URI for the app on the pair path", async () => {
  delete sessionStore[PAIR_URI_KEY];
  await handlePairingUri(REAL_WITH_PRERELEASE, EXAMPLE_MANIFEST);
  assert.equal(sessionStore[PAIR_URI_KEY], REAL_WITH_PRERELEASE);
});

test("handlePairingUri stashes the INNER tyde-pair URI from the HTTPS QR form", async () => {
  // The generic HTTPS QR carries the pairing URI in its fragment. The loader
  // must normalize it down to the raw `tyde-pair://…` string the WASM app's
  // `take_pending_pairing_uri` understands — never the wrapping https URL.
  delete sessionStore[PAIR_URI_KEY];
  const url = `https://tycode.dev/tyde/#${REAL_WITH_PRERELEASE}`;
  await handlePairingUri(url, EXAMPLE_MANIFEST);
  assert.equal(sessionStore[PAIR_URI_KEY], REAL_WITH_PRERELEASE);
});

test("handlePairingUri does NOT stash when the URI is not a pairing code", async () => {
  delete sessionStore[PAIR_URI_KEY];
  await handlePairingUri("https://evil.example/", EXAMPLE_MANIFEST);
  assert.equal(sessionStore[PAIR_URI_KEY], undefined);
});

test("handlePairingUri does NOT stash when the version is not in the manifest", async () => {
  delete sessionStore[PAIR_URI_KEY];
  // A synthetic URI whose release_version is a valid semver absent from the manifest.
  const uri = makePairingUri({ v: 2, protocol_version: 13, release_version: "99.99.99" });
  await handlePairingUri(uri, EXAMPLE_MANIFEST);
  assert.equal(sessionStore[PAIR_URI_KEY], undefined);
});

// --- protocol drift: fail closed BEFORE booting (consensus #1) --------------

test("handlePairingUri does NOT stash/boot when the bundle protocol mismatches the host QR", async () => {
  delete sessionStore[PAIR_URI_KEY];
  // QR is protocol 13; published bundle claims protocol 99 → drift.
  await handlePairingUri(REAL_WITH_PRERELEASE, manifestWithBetaProtocol(99));
  assert.equal(sessionStore[PAIR_URI_KEY], undefined, "no stash on protocol drift");
});

test("handlePairingUri does NOT stash/boot when the bundle has no protocol metadata", async () => {
  delete sessionStore[PAIR_URI_KEY];
  // Packaging drift: the published version record was never stamped with a protocol.
  await handlePairingUri(REAL_WITH_PRERELEASE, manifestWithBetaProtocol(undefined));
  assert.equal(sessionStore[PAIR_URI_KEY], undefined, "no stash when protocol unpublished");
});

test("handlePairingUri still stashes when the bundle protocol matches the host QR", async () => {
  delete sessionStore[PAIR_URI_KEY];
  await handlePairingUri(REAL_WITH_PRERELEASE, manifestWithBetaProtocol(13));
  assert.equal(sessionStore[PAIR_URI_KEY], REAL_WITH_PRERELEASE);
});

// --- checkProtocolCompatibility (pure) --------------------------------------

test("checkProtocolCompatibility: equal protocols pass", () => {
  assert.deepEqual(checkProtocolCompatibility(13, { protocolVersion: 13 }), { ok: true });
});

test("checkProtocolCompatibility: mismatch fails with protocol-mismatch", () => {
  const r = checkProtocolCompatibility(21, { protocolVersion: 19 });
  assert.equal(r.ok, false);
  assert.equal(r.reason, "protocol-mismatch");
  assert.match(r.detail, /21/);
  assert.match(r.detail, /19/);
});

test("checkProtocolCompatibility: missing bundle protocol fails with protocol-unpublished", () => {
  assert.equal(checkProtocolCompatibility(21, { protocolVersion: null }).reason, "protocol-unpublished");
  assert.equal(checkProtocolCompatibility(21, {}).reason, "protocol-unpublished");
});

test("checkProtocolCompatibility: non-integer QR protocol fails closed", () => {
  assert.equal(checkProtocolCompatibility(null, { protocolVersion: 19 }).reason, "protocol-mismatch");
  assert.equal(checkProtocolCompatibility(undefined, { protocolVersion: 19 }).reason, "protocol-mismatch");
});

// --- self-heal handoff: takeRepairUri + onRepairNeeded (consensus #2) -------

test("takeRepairUri reads and clears the stashed repair URI", () => {
  sessionStore[REPAIR_URI_KEY] = REAL_WITH_PRERELEASE;
  assert.equal(takeRepairUri(), REAL_WITH_PRERELEASE);
  assert.equal(sessionStore[REPAIR_URI_KEY], undefined, "cleared after read");
  assert.equal(takeRepairUri(), null, "second read is empty");
});

test("onRepairNeeded with a URI forgets the stale version, stashes for reload, and reloads", () => {
  delete sessionStore[REPAIR_URI_KEY];
  localStore[STORAGE_KEY] = "0.8.19-beta.8"; // stale remembered version
  let reloads = 0;
  const routed = onRepairNeeded(REAL_WITH_PRERELEASE, () => {
    reloads += 1;
  });
  assert.equal(routed, true);
  assert.equal(localStore[STORAGE_KEY], undefined, "stale remembered version forgotten");
  assert.equal(sessionStore[REPAIR_URI_KEY], REAL_WITH_PRERELEASE, "URI stashed for the fresh loader");
  assert.equal(reloads, 1, "reloaded to tear down the stale WASM");
});

test("onRepairNeeded without a URI forgets the version and does not reload", () => {
  delete sessionStore[REPAIR_URI_KEY];
  localStore[STORAGE_KEY] = "0.8.19-beta.8";
  let reloads = 0;
  const routed = onRepairNeeded(null, () => {
    reloads += 1;
  });
  assert.equal(routed, false);
  assert.equal(localStore[STORAGE_KEY], undefined, "version still forgotten");
  assert.equal(sessionStore[REPAIR_URI_KEY], undefined, "nothing stashed");
  assert.equal(reloads, 0, "no reload without a carried URI");
});

// --- reconnect self-heal: takeRepairVersion + onRepairVersion --------------

test("takeRepairVersion reads and clears the stashed repair version", () => {
  sessionStore[REPAIR_VERSION_KEY] = "0.8.19-beta.2";
  assert.equal(takeRepairVersion(), "0.8.19-beta.2");
  assert.equal(sessionStore[REPAIR_VERSION_KEY], undefined, "cleared after read");
  assert.equal(takeRepairVersion(), null, "second read is empty");
});

test("onRepairVersion with a version forgets the stale bundle, stashes it, and reloads", () => {
  delete sessionStore[REPAIR_VERSION_KEY];
  localStore[STORAGE_KEY] = "0.8.19-beta.8"; // stale remembered version
  let reloads = 0;
  const routed = onRepairVersion("0.8.19-beta.2", () => {
    reloads += 1;
  });
  assert.equal(routed, true);
  assert.equal(localStore[STORAGE_KEY], undefined, "stale remembered version forgotten");
  assert.equal(
    sessionStore[REPAIR_VERSION_KEY],
    "0.8.19-beta.2",
    "version stashed for the fresh loader",
  );
  assert.equal(reloads, 1, "reloaded to tear down the stale WASM");
});

test("onRepairVersion without a version forgets the bundle and does not reload", () => {
  delete sessionStore[REPAIR_VERSION_KEY];
  localStore[STORAGE_KEY] = "0.8.19-beta.8";
  let reloads = 0;
  const routed = onRepairVersion(null, () => {
    reloads += 1;
  });
  assert.equal(routed, false);
  assert.equal(localStore[STORAGE_KEY], undefined, "version still forgotten");
  assert.equal(sessionStore[REPAIR_VERSION_KEY], undefined, "nothing stashed");
  assert.equal(reloads, 0, "no reload without a carried version");
});

// --- reload-loop breaker (Claude review #1) ---------------------------------

test("registerRepairAttempt increments and persists the session counter", () => {
  delete sessionStore[REPAIR_ATTEMPTS_KEY];
  assert.equal(registerRepairAttempt(), 1);
  assert.equal(registerRepairAttempt(), 2);
  assert.equal(registerRepairAttempt(), 3);
  assert.equal(sessionStore[REPAIR_ATTEMPTS_KEY], "3", "persisted across calls");
});

test("clearRepairAttempts resets the counter so a future drift can self-heal", () => {
  sessionStore[REPAIR_ATTEMPTS_KEY] = "5";
  clearRepairAttempts();
  assert.equal(sessionStore[REPAIR_ATTEMPTS_KEY], undefined);
  assert.equal(registerRepairAttempt(), 1, "counts from 1 again after a reset");
});

test("registerRepairAttempt recovers from a corrupt counter value", () => {
  sessionStore[REPAIR_ATTEMPTS_KEY] = "not-a-number";
  assert.equal(registerRepairAttempt(), 1);
});

test("the loop breaker trips on the reboot after MAX_REPAIR_ATTEMPTS", () => {
  // Mirror init()'s guard: it boots while attempts <= MAX and breaks once the
  // count exceeds it, so a corrupt manifest can reboot at most MAX_REPAIR_ATTEMPTS
  // times before showing an explicit error instead of reloading forever.
  delete sessionStore[REPAIR_ATTEMPTS_KEY];
  let boots = 0;
  let broke = false;
  for (let reload = 0; reload < 10 && !broke; reload++) {
    const attempts = registerRepairAttempt();
    if (attempts > MAX_REPAIR_ATTEMPTS) {
      clearRepairAttempts();
      broke = true;
    } else {
      boots += 1; // would call handlePairingUri(repairUri) and reboot
    }
  }
  assert.equal(broke, true, "loop is broken, not infinite");
  assert.equal(boots, MAX_REPAIR_ATTEMPTS, "at most MAX_REPAIR_ATTEMPTS reboots");
  assert.equal(sessionStore[REPAIR_ATTEMPTS_KEY], undefined, "repair state cleared on break");
  assert.ok(MAX_REPAIR_ATTEMPTS >= 1 && MAX_REPAIR_ATTEMPTS <= 2, "small guard (1–2)");
});

// --- pending-fragment recovery across a retry reload (Codex blocker) --------

test("resolvePairingFragment captures a URL fragment and mirrors it for retry", () => {
  delete sessionStore[PENDING_FRAGMENT_KEY];
  const result = resolvePairingFragment(`#${REAL_WITH_PRERELEASE}`);
  assert.equal(result, REAL_WITH_PRERELEASE, "returns the captured fragment");
  assert.equal(
    sessionStore[PENDING_FRAGMENT_KEY],
    REAL_WITH_PRERELEASE,
    "mirrors the fragment to sessionStorage so a retry reload can recover it",
  );
});

test("resolvePairingFragment recovers the stashed fragment when the URL has none", () => {
  // Simulates a retry reload: the URL fragment was already captured + cleared on
  // the first load, so the hash is now empty.
  sessionStore[PENDING_FRAGMENT_KEY] = REAL_WITH_PRERELEASE;
  assert.equal(resolvePairingFragment(""), REAL_WITH_PRERELEASE);
  assert.equal(resolvePairingFragment(undefined), REAL_WITH_PRERELEASE);
});

test("resolvePairingFragment returns null with no URL fragment and nothing stashed", () => {
  delete sessionStore[PENDING_FRAGMENT_KEY];
  assert.equal(resolvePairingFragment(""), null);
});

test("resolvePairingFragment ignores a non-pairing URL fragment and persists nothing", () => {
  delete sessionStore[PENDING_FRAGMENT_KEY];
  assert.equal(resolvePairingFragment("#section-2"), null);
  assert.equal(sessionStore[PENDING_FRAGMENT_KEY], undefined);
});

// End-to-end blocker: QR is cleared before async work, a manifest fetch fails,
// and the retry recovers the original pairing URI WITHOUT a rescan.
test("a failed manifest fetch does not lose the pairing URI; retry recovers it", async () => {
  delete sessionStore[PENDING_FRAGMENT_KEY];
  delete sessionStore[PAIR_URI_KEY];

  // Load 1: init() captures the URL fragment (mirroring it for retry) and clears
  // the URL. Then the manifest fetch FAILS, so init() returns at setError
  // BEFORE ever calling handlePairingUri — the pending copy must survive.
  const captured = resolvePairingFragment(`#${REAL_WITH_PRERELEASE}`);
  assert.equal(captured, REAL_WITH_PRERELEASE);
  assert.equal(
    sessionStore[PENDING_FRAGMENT_KEY],
    REAL_WITH_PRERELEASE,
    "pending pairing URI survives a failed manifest fetch",
  );

  // Retry = reload: the URL no longer has the fragment, but init() recovers it
  // from sessionStorage instead of forcing a rescan.
  const recovered = resolvePairingFragment("");
  assert.equal(recovered, REAL_WITH_PRERELEASE, "retry recovers the original URI");

  // This time the manifest fetch succeeds and pairing commits: the URI is handed
  // to the app (PAIR_URI_KEY) and the pending recovery copy is consumed.
  await handlePairingUri(recovered, EXAMPLE_MANIFEST);
  assert.equal(sessionStore[PAIR_URI_KEY], REAL_WITH_PRERELEASE, "app handoff stashed");
  assert.equal(
    sessionStore[PENDING_FRAGMENT_KEY],
    undefined,
    "pending fragment consumed once a URI is committed to boot",
  );
});

test("handlePairingUri consumes the pending fragment on commit", async () => {
  sessionStore[PENDING_FRAGMENT_KEY] = REAL_WITH_PRERELEASE;
  delete sessionStore[PAIR_URI_KEY];
  await handlePairingUri(REAL_WITH_PRERELEASE, EXAMPLE_MANIFEST);
  assert.equal(
    sessionStore[PENDING_FRAGMENT_KEY],
    undefined,
    "committing a URI clears the pending recovery copy (no stale replay)",
  );
});
