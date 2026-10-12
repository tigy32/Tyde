import { test } from "node:test";
import assert from "node:assert/strict";
import { Miniflare } from "miniflare";
import { fileURLToPath } from "node:url";
import { randomBytes, createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { Relay, HostRegistration } from "../codec.mjs";

const random = () => randomBytes(32).toString("hex");
const hash = code => createHash("sha256").update(code).digest("hex");

test("Real worker isolates paired hosts and enforces OAuth lifetime and revocation", async () => {
  const binary = fileURLToPath(new URL(`../../../target/debug/tyde-server${process.platform === "win32" ? ".exe" : ""}`, import.meta.url));
  const schema = execFileSync(binary, ["dots-schema"], { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
  execFileSync("python3", [fileURLToPath(new URL("../../../tools/generate-dots-codec.py", import.meta.url)), "--check"], { input: schema, stdio: ["pipe", "pipe", "pipe"] });
  const options = { modules: true, modulesRoot: fileURLToPath(new URL("../", import.meta.url)), scriptPath: fileURLToPath(new URL("../worker.mjs", import.meta.url)),
    compatibilityDate: "2026-07-30", durableObjects: { BROKER: { className: "DotsBroker", useSQLite: true } }, port: 0 };
  const server = new Miniflare(options);
  const origin = (await server.ready).origin;
  const request = (path, options) => fetch(`${origin}${path}`, options);
  const post = (path, data) => request(path, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(data) });
  const registerHost = (id, key, pairing) => post("/hosts", JSON.parse(HostRegistration.encode(id, key, pairing)));
  const form = (path, data) => request(path, { method: "POST", redirect: "manual", headers: { "Content-Type": "application/x-www-form-urlencoded", Origin: origin }, body: new URLSearchParams(data) });
  const sockets = [];
  try {
    assert.equal((await request("/mcp", { method: "POST", body: "{}" })).status, 401);
    const discovery = await (await request("/.well-known/oauth-authorization-server")).json();
    assert.equal(discovery.issuer, origin);
    assert.deepEqual(discovery.code_challenge_methods_supported, ["S256"]);
    assert.equal((await post("/oauth/register", { redirect_uris: ["https://attacker.invalid/callback"] })).status, 400);
    const callback = "https://chatgpt.com/connector_platform_oauth_redirect";
    const client = await (await post("/oauth/register", { redirect_uris: [callback], token_endpoint_auth_method: "none" })).json();
    const hosts = [];
    for (let i = 0; i < 2; i++) {
      const host = { host_id: random(), host_key: random(), pairing_code: random() };
      const response = await registerHost(host.host_id, host.host_key, hash(host.pairing_code));
      assert.equal(response.status, 200);
      const connection = await server.dispatchFetch(`${origin}/hosts/${host.host_id}/connect`, { headers: { Upgrade: "websocket", Authorization: `Bearer ${host.host_key}` } });
      const socket = connection.webSocket;
      socket.accept(); sockets.push(socket);
      host.socket = socket;
      hosts.push(host);
    }
    assert.equal((await registerHost(hosts[0].host_id, random(), hash(random()))).status, 401);
    const verifier = random();
    const challenge = createHash("sha256").update(verifier).digest("base64url");
    const auth = { client_id: client.client_id, redirect_uri: callback, resource: `${origin}/mcp`, response_type: "code", code_challenge_method: "S256", code_challenge: challenge, scope: "tyde:control", state: random() };
    const page = await (await request(`/oauth/authorize?${new URLSearchParams(auth)}`)).text();
    const transaction = /name="transaction" value="([a-f0-9]+)"/.exec(page)?.[1];
    assert.ok(transaction, "Authorization page supplies a transaction");
    assert.equal((await form("/oauth/authorize", { transaction, pairing_code: random() })).status, 400);
    const accepted = await form("/oauth/authorize", { transaction, pairing_code: hosts[0].pairing_code });
    assert.equal(accepted.status, 302);
    const redirect = new URL(accepted.headers.get("Location"));
    assert.equal(redirect.origin, "https://chatgpt.com");
    assert.equal(redirect.searchParams.get("iss"), origin);
    assert.equal((await form("/oauth/authorize", { transaction, pairing_code: hosts[0].pairing_code })).status, 400);
    const exchange = { grant_type: "authorization_code", code: redirect.searchParams.get("code"), client_id: client.client_id, redirect_uri: callback, resource: `${origin}/mcp`, code_verifier: verifier };
    assert.equal((await form("/oauth/token", { ...exchange, code_verifier: random() })).status, 400);
    assert.equal((await form("/oauth/token", { ...exchange, resource: "https://other.invalid/mcp" })).status, 400);
    const tokens = await (await form("/oauth/token", exchange)).json();
    assert.ok(tokens.access_token, "OAuth issues an access token");
    assert.equal((await form("/oauth/token", exchange)).status, 400);
    let otherReceived = false;
    hosts[1].socket.addEventListener("message", () => { otherReceived = true; });
    hosts[0].socket.addEventListener("message", event => {
      const frame = Relay.decode(event.data);
      if (Relay.isRequest(frame)) hosts[0].socket.send(Relay.response(JSON.stringify({ relay: "first-host" }), 200, frame.transport_id));
    });
    const result = await request("/mcp", { method: "POST", headers: { Authorization: `Bearer ${tokens.access_token}` }, body: "{}" });
    assert.equal(result.status, 200);
    assert.deepEqual(await result.json(), { relay: "first-host" });
    assert.equal(otherReceived, false, "The other host receives no command");
    const refresh = { grant_type: "refresh_token", client_id: client.client_id, resource: `${origin}/mcp`, refresh_token: tokens.refresh_token };
    const rotatedResponse = await form("/oauth/token", refresh);
    assert.equal(rotatedResponse.status, 200);
    const rotated = await rotatedResponse.json();
    assert.ok(rotated.access_token && rotated.refresh_token, "Rotation supplies both replacement credentials");
    assert.equal((await form("/oauth/token", refresh)).status, 400);
    const rotatedResult = await request("/mcp", { method: "POST", headers: { Authorization: `Bearer ${rotated.access_token}` }, body: "{}" });
    assert.equal(rotatedResult.status, 200);
    assert.deepEqual(await rotatedResult.json(), { relay: "first-host" });
    const rotatedRefresh = { ...refresh, refresh_token: rotated.refresh_token };
    const currentResponse = await form("/oauth/token", rotatedRefresh);
    assert.equal(currentResponse.status, 200, "A replacement refresh token can rotate again");
    const current = await currentResponse.json();
    assert.equal((await form("/oauth/token", rotatedRefresh)).status, 400, "Replacement refresh tokens are also single-use");
    assert.equal((await request(`/hosts/${hosts[0].host_id}`, { method: "DELETE", headers: { Authorization: `Bearer ${hosts[0].host_key}` } })).status, 200);
    assert.equal((await request("/mcp", { method: "POST", headers: { Authorization: `Bearer ${tokens.access_token}` }, body: "{}" })).status, 401);
    assert.equal((await request("/mcp", { method: "POST", headers: { Authorization: `Bearer ${current.access_token}` }, body: "{}" })).status, 401, "Revocation invalidates the current access token");
    assert.equal((await form("/oauth/token", { ...refresh, refresh_token: current.refresh_token })).status, 400, "Revocation invalidates the current refresh token");

    let offset = 0;
    const advance = async milliseconds => {
      offset += milliseconds;
      await server.setOptions({ ...options, port: Number(new URL(origin).port), bindings: { TEST_TIME_OFFSET: String(offset) } });
      assert.equal((await server.ready).origin, origin, "Clock changes retain the same real HTTP origin");
    };
    const newHost = async () => {
      const host = { host_id: random(), host_key: random(), pairing_code: random() };
      assert.equal((await registerHost(host.host_id, host.host_key, hash(host.pairing_code))).status, 200);
      return host;
    };
    const transactionFor = async () => {
      const response = await request(`/oauth/authorize?${new URLSearchParams({ ...auth, state: random() })}`);
      assert.equal(response.status, 200);
      return /name="transaction" value="([a-f0-9]+)"/.exec(await response.text())[1];
    };
    const approve = async host => {
      const response = await form("/oauth/authorize", { transaction: await transactionFor(), pairing_code: host.pairing_code });
      assert.equal(response.status, 302);
      return new URL(response.headers.get("Location")).searchParams.get("code");
    };
    const exchangeCode = code => form("/oauth/token", { ...exchange, code });
    const expiredHost = await newHost();
    const staleTransaction = await transactionFor();
    await advance(601000);
    const stalePairing = await form("/oauth/authorize", { transaction: await transactionFor(), pairing_code: expiredHost.pairing_code });
    assert.equal(stalePairing.status, 400);
    assert.equal((await stalePairing.json()).error, "invalid_pairing_code");
    expiredHost.pairing_code = random();
    assert.equal((await registerHost(expiredHost.host_id, expiredHost.host_key, hash(expiredHost.pairing_code))).status, 200);
    const staleAuthorization = await form("/oauth/authorize", { transaction: staleTransaction, pairing_code: expiredHost.pairing_code });
    assert.equal(staleAuthorization.status, 400);
    assert.equal((await staleAuthorization.json()).error, "authorization_expired");
    const expiringCode = await approve(expiredHost);
    await advance(61000);
    assert.equal((await exchangeCode(expiringCode)).status, 400, "Expired authorization code must be rejected");
    const tokenHost = await newHost();
    const fresh = await (await exchangeCode(await approve(tokenHost))).json();
    assert.ok(fresh.access_token, "A fresh grant still works after clock advances");
    const freshRequest = () => request("/mcp", { method: "POST", headers: { Authorization: `Bearer ${fresh.access_token}` }, body: "{}" });
    assert.equal((await freshRequest()).status, 503, "Fresh token is authorized but this host is intentionally offline");
    await advance(3601000);
    assert.equal((await freshRequest()).status, 401, "Expired access token must not reach host routing");
    await advance(2592001000);
    assert.equal((await form("/oauth/token", { ...refresh, refresh_token: fresh.refresh_token })).status, 400, "Expired refresh token must not issue a new grant");
  } finally { for (const socket of sockets) socket.close(); await server.dispose(); }
});
