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
  // Clock changes restart workerd; pooled sockets from the previous runtime
  // otherwise race its exit and reset the first request after server.ready.
  const request = (path, options = {}) => fetch(`${origin}${path}`, {
    ...options, headers: { ...options.headers, Connection: "close" }
  });
  const post = (path, data) => request(path, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(data) });
  const registerHost = (id, key, pairing) => post("/hosts", JSON.parse(HostRegistration.encode(id, key, pairing)));
  const form = (path, data) => request(path, { method: "POST", redirect: "manual", headers: { "Content-Type": "application/x-www-form-urlencoded", Origin: origin }, body: new URLSearchParams(data) });
  const sockets = [];
  try {
    assert.equal((await request("/mcp", { method: "POST", body: "{}" })).status, 401);
    const discovery = await (await request("/.well-known/oauth-authorization-server")).json();
    assert.equal(discovery.issuer, origin);
    assert.deepEqual(discovery.code_challenge_methods_supported, ["S256"]);
    assert.equal((await post("/oauth/register", { redirect_uris: ["http://attacker.invalid/callback"] })).status, 400);
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
    const authorizationResponse = await request(`/oauth/authorize?${new URLSearchParams(auth)}`);
    assert.equal(authorizationResponse.status, 200);
    assert.ok(/^text\/html(?:;|$)/i.test(authorizationResponse.headers.get("Content-Type")), "Authorization is delivered as HTML");
    assert.deepEqual(authorizationResponse.headers.get("Cache-Control").split(",").map(value => value.trim()), ["no-store", "no-transform"], "Consent stays private and blocks Cloudflare edge analytics injection");
    const page = await authorizationResponse.text();
    const attribute = (tag, name) => {
      const match = new RegExp(`(?:^|\\s)${name}\\s*=\\s*(?:"([^"]*)"|'([^']*)')`, "i").exec(tag);
      return match?.[1] ?? match?.[2];
    };
    const text = markup => markup.replace(/<[^>]*>/g, " ").replace(/\s+/g, " ").trim();
    const authorizationForm = /<form\b([^>]*)>([\s\S]*?)<\/form>/i.exec(page);
    assert.ok(authorizationForm, "Authorization page supplies a form");
    assert.equal(attribute(authorizationForm[1], "method")?.toLowerCase(), "post", "Authorization uses POST");
    assert.equal(attribute(authorizationForm[1], "action"), "/oauth/authorize");
    const pairingInput = [...authorizationForm[2].matchAll(/<input\b[^>]*>/gi)]
      .map(match => match[0]).find(tag => attribute(tag, "name") === "pairing_code");
    assert.ok(pairingInput, "Authorization form accepts the pairing code");
    const pairingId = attribute(pairingInput, "id");
    const pairingLabel = [...authorizationForm[2].matchAll(/<label\b([^>]*)>([\s\S]*?)<\/label>/gi)]
      .find(([, tag, contents]) => contents.includes(pairingInput) || (pairingId && attribute(tag, "for") === pairingId));
    assert.ok(pairingLabel && /pairing\s+code/i.test(text(pairingLabel[2])), "Pairing code has an associated, readable label");
    const authorizeButton = [...authorizationForm[2].matchAll(/<button\b([^>]*)>([\s\S]*?)<\/button>/gi)]
      .find(([, tag, contents]) => attribute(tag, "type")?.toLowerCase() === "submit" && /authorize\s+chatgpt/i.test(text(contents)));
    assert.ok(authorizeButton, "Authorization form has an Authorize ChatGPT submit button");
    const disclosure = text(page);
    assert.ok(/ChatGPT and your dot access to this host's projects and agents/i.test(disclosure), "Disclosure identifies who gains host access");
    assert.ok(/modify\s+files/i.test(disclosure), "Disclosure includes file modification access");
    assert.ok(/configured\s+providers/i.test(disclosure), "Disclosure includes configured provider access");
    assert.ok(/only connect a host you control/i.test(disclosure), "Disclosure retains the host ownership warning");

    const stylesheet = [...page.matchAll(/<link\b[^>]*>/gi)]
      .map(match => match[0]).find(tag => attribute(tag, "rel")?.toLowerCase().split(/\s+/).includes("stylesheet"));
    assert.ok(stylesheet, "Authorization page links its stylesheet");
    const stylesheetUrl = new URL(attribute(stylesheet, "href"), origin);
    assert.equal(stylesheetUrl.origin, origin, "Authorization stylesheet is same-origin");
    assert.equal(stylesheetUrl.pathname, "/oauth/style.css");
    const stylesheetResponse = await request(`${stylesheetUrl.pathname}${stylesheetUrl.search}`);
    assert.equal(stylesheetResponse.status, 200, "The linked stylesheet is served over real HTTP");
    assert.ok(/^text\/css(?:;|$)/i.test(stylesheetResponse.headers.get("Content-Type")), "The stylesheet is delivered as CSS");
    assert.ok((await stylesheetResponse.text()).trim().length > 0, "The stylesheet is not empty");
    const csp = authorizationResponse.headers.get("Content-Security-Policy");
    assert.ok(csp, "Authorization page retains its security policy");
    const directives = new Map(csp.split(";").map(directive => directive.trim().split(/\s+/)).filter(([name]) => name)
      .map(([name, ...sources]) => [name, sources]));
    assert.deepEqual(directives.get("default-src"), ["'none'"]);
    assert.deepEqual(directives.get("style-src"), ["'self'"], "CSP permits only same-origin stylesheets");
    assert.deepEqual(directives.get("form-action"), ["'self'", "https://chatgpt.com"], "CSP permits the registered callback redirect, not arbitrary destinations");
    assert.equal(authorizationResponse.headers.get("Referrer-Policy"), "same-origin", "Same-origin referrers retain a real browser POST Origin without leaking authorization URLs to the client");
    assert.deepEqual(directives.get("frame-ancestors"), ["'none'"], "The authorization page cannot be framed");
    assert.equal(csp.includes("'unsafe-inline'"), false, "Styling must not allow inline code");
    for (const directive of ["script-src", "script-src-elem", "script-src-attr", "frame-src", "child-src"]) {
      if (directives.has(directive)) assert.deepEqual(directives.get(directive), ["'none'"], `${directive} remains blocked`);
    }
    if (directives.has("style-src-elem")) assert.deepEqual(directives.get("style-src-elem"), ["'self'"]);
    if (directives.has("style-src-attr")) assert.deepEqual(directives.get("style-src-attr"), ["'none'"]);
    const transaction = /name="transaction" value="([a-f0-9]+)"/.exec(page)?.[1];
    assert.ok(transaction, "Authorization page supplies a transaction");
    for (const badOrigin of ["null", "https://attacker.invalid"]) {
      const rejected = await request("/oauth/authorize", { method: "POST", redirect: "manual", headers: { "Content-Type": "application/x-www-form-urlencoded", Origin: badOrigin }, body: new URLSearchParams({ transaction, pairing_code: hosts[0].pairing_code }) });
      assert.equal(rejected.status, 403, "Null or foreign browser origins cannot authorize a host");
    }
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

    for (const redirect of ["javascript:alert(1)", "https://user:pass@mcp-client.example/callback", "https://mcp-client.example/callback#fragment", "http://127.0.0.1:8123/callback"]) {
      assert.equal((await post("/oauth/register", { redirect_uris: [redirect] })).status, 400, "Unsafe callbacks and undeclared native callbacks are rejected");
    }
    for (const redirect of ["http://attacker.invalid/callback", "http://192.168.1.5:8123/callback", "http://localhost.attacker.invalid/callback"]) {
      assert.equal((await post("/oauth/register", { redirect_uris: [redirect], application_type: "native" })).status, 400, "Declared native clients cannot send authorization codes over non-loopback HTTP");
    }
    assert.equal((await post("/oauth/register", { redirect_uris: ["http://127.0.0.1:8123/callback"], application_type: "web" })).status, 400);
    const genericCallback = "https://mcp-client.example/callback?workspace=browser-flow";
    const differentCallback = new URL(genericCallback);
    differentCallback.pathname += "/other";
    for (const [redirect_uri, application_type] of [[genericCallback, "web"], ["http://127.0.0.1:8123/callback", "native"], ["http://[::1]:8123/callback", "native"]]) {
      assert.equal((await post("/oauth/register", { redirect_uris: [redirect_uri], application_type, token_endpoint_auth_method: "none" })).status, 201, "HTTPS clients and explicit native loopback clients can register");
    }
    const genericClient = await (await post("/oauth/register", { redirect_uris: [genericCallback], token_endpoint_auth_method: "none" })).json();
    const genericAuth = { ...auth, client_id: genericClient.client_id, redirect_uri: genericCallback, state: random() };
    const genericAuthorize = parameters => request(`/oauth/authorize?${new URLSearchParams(parameters)}`);
    assert.equal((await genericAuthorize({ ...genericAuth, redirect_uri: differentCallback.href })).status, 400, "Even a valid HTTPS callback must match its registered URI exactly");
    const genericResponse = await genericAuthorize(genericAuth);
    assert.equal(genericResponse.status, 200);
    assert.ok(genericResponse.headers.get("Content-Security-Policy").includes("form-action 'self' https://mcp-client.example;"), "Only the registered client's callback origin is permitted");
    const genericPage = await genericResponse.text();
    assert.ok(/Authorize MCP client/.test(genericPage), "Non-ChatGPT clients receive honest consent labeling");
    assert.ok(genericPage.includes("https://mcp-client.example"), "Consent exposes the actual callback origin");
    const genericTransaction = /name="transaction" value="([a-f0-9]+)"/.exec(genericPage)[1];
    const genericApproved = await form("/oauth/authorize", { transaction: genericTransaction, pairing_code: hosts[1].pairing_code });
    assert.equal(genericApproved.status, 302);
    const genericRedirect = new URL(genericApproved.headers.get("Location"));
    assert.equal(genericRedirect.origin, new URL(genericCallback).origin);
    assert.equal(genericRedirect.pathname, new URL(genericCallback).pathname, "Authorization returns to the exact registered callback handler");
    for (const [name, value] of new URL(genericCallback).searchParams) {
      assert.equal(genericRedirect.searchParams.get(name), value, "Registered client query parameters survive authorization");
    }
    assert.ok(genericRedirect.searchParams.get("state") === genericAuth.state, "The generic client receives its original OAuth state");
    const genericExchange = { ...exchange, code: genericRedirect.searchParams.get("code"), client_id: genericClient.client_id, redirect_uri: genericCallback };
    assert.equal((await form("/oauth/token", { ...genericExchange, redirect_uri: differentCallback.href })).status, 400, "Callback binding is enforced again during exchange");
    assert.equal((await form("/oauth/token", { ...genericExchange, client_id: client.client_id })).status, 400, "A ChatGPT client cannot redeem another client's authorization");
    const genericTokenResponse = await form("/oauth/token", genericExchange);
    assert.equal(genericTokenResponse.status, 200, "A generic client exchanges its own authorization code");
    const genericTokens = await genericTokenResponse.json();
    assert.ok(genericTokens.access_token, "A generic MCP client receives its own grant");
    hosts[1].socket.addEventListener("message", event => {
      const frame = Relay.decode(event.data);
      if (Relay.isRequest(frame)) hosts[1].socket.send(Relay.response(JSON.stringify({ relay: "second-host" }), 200, frame.transport_id));
    });
    const genericResult = await request("/mcp", { method: "POST", headers: { Authorization: `Bearer ${genericTokens.access_token}` }, body: "{}" });
    assert.equal(genericResult.status, 200);
    assert.deepEqual(await genericResult.json(), { relay: "second-host" });
    assert.equal((await request(`/hosts/${hosts[1].host_id}`, { method: "DELETE", headers: { Authorization: `Bearer ${hosts[1].host_key}` } })).status, 200);
    assert.equal((await request("/mcp", { method: "POST", headers: { Authorization: `Bearer ${genericTokens.access_token}` }, body: "{}" })).status, 401, "Generic client grants are revoked at the same host boundary");

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
