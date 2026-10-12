import { DurableObject } from "cloudflare:workers";
import { Relay, HostRegistration } from "./codec.mjs";

const SCOPE = "tyde:control";
const MAX_BODY = 262144;
const json = (value, status = 200, headers = {}) => new Response(JSON.stringify(value), {
  status, headers: { "Content-Type": "application/json", "Cache-Control": "no-store", ...headers },
});
const random = () => Array.from(crypto.getRandomValues(new Uint8Array(32)), b => b.toString(16).padStart(2, "0")).join("");
const hash = async value => Array.from(new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value))), b => b.toString(16).padStart(2, "0")).join("");
const pkce = async value => btoa(String.fromCharCode(...new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value))))).replaceAll("+", "-").replaceAll("/", "_").replaceAll("=", "");
const escape = value => value.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll('"', "&quot;").replaceAll("'", "&#39;");
const callbackAllowed = value => {
  try {
    const url = new URL(value);
    return url.origin === "https://chatgpt.com" && !url.search && !url.hash &&
      (url.pathname === "/connector_platform_oauth_redirect" || /^\/connector\/oauth\/[A-Za-z0-9_-]+$/.test(url.pathname));
  } catch { return false; }
};

async function body(request) {
  const reader = request.body?.getReader();
  if (!reader) return "";
  const chunks = [];
  let size = 0;
  for (;;) {
    const part = await reader.read();
    if (part.done) break;
    size += part.value.length;
    if (size > MAX_BODY) { await reader.cancel(); throw new Error("Request too large"); }
    chunks.push(part.value);
  }
  const result = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) { result.set(chunk, offset); offset += chunk.length; }
  return new TextDecoder().decode(result);
}

export default {
  async fetch(request, env) {
    try {
      return await env.BROKER.get(env.BROKER.idFromName("dots")).fetch(request);
    } catch {
      // OAuth inputs and MCP payloads can contain credentials or user content.
      console.error("Dots service request failed", { method: request.method });
      return json({ error: "service_unavailable" }, 503);
    }
  },
};

export class DotsBroker extends DurableObject {
  constructor(ctx, env) {
    super(ctx, env);
    this.pending = new Map();
  }

  async fetch(request) {
    const url = new URL(request.url);
    const origin = url.origin;
    const resource = `${origin}/mcp`;
    if (this.env.TEST_TIME_OFFSET !== undefined && !["127.0.0.1", "localhost"].includes(url.hostname)) throw new Error("Test clock is not available on a public origin");
    const now = this.time();
    if (url.pathname === "/healthz") return json({ status: "ok" });
    if (url.pathname === "/.well-known/oauth-protected-resource" || url.pathname === "/.well-known/oauth-protected-resource/mcp") {
      return json({ resource, authorization_servers: [origin], scopes_supported: [SCOPE] });
    }
    if (url.pathname === "/.well-known/oauth-authorization-server") {
      return json({ issuer: origin, authorization_endpoint: `${origin}/oauth/authorize`, token_endpoint: `${origin}/oauth/token`,
        registration_endpoint: `${origin}/oauth/register`, response_types_supported: ["code"],
        grant_types_supported: ["authorization_code", "refresh_token"], token_endpoint_auth_methods_supported: ["none"],
        code_challenge_methods_supported: ["S256"], scopes_supported: [SCOPE], authorization_response_iss_parameter_supported: true });
    }
    if (url.pathname === "/oauth/register" && request.method === "POST") {
      if (!await this.rate(request, 30)) return json({ error: "rate_limited" }, 429);
      const input = JSON.parse(await body(request));
      if (!Array.isArray(input.redirect_uris) || !input.redirect_uris.length || input.redirect_uris.length > 4 ||
          !input.redirect_uris.every(callbackAllowed) || (input.token_endpoint_auth_method && input.token_endpoint_auth_method !== "none")) {
        return json({ error: "invalid_client_metadata" }, 400);
      }
      const client_id = random();
      await this.ctx.storage.put(`client:${client_id}`, { redirect_uris: input.redirect_uris });
      return json({ client_id, redirect_uris: input.redirect_uris, token_endpoint_auth_method: "none", grant_types: ["authorization_code", "refresh_token"] }, 201);
    }
    if (url.pathname === "/hosts" && request.method === "POST") {
      if (!await this.rate(request, 10)) return json({ error: "rate_limited" }, 429);
      let input;
      try { input = HostRegistration.decode(await body(request)); }
      catch { return json({ error: "invalid_registration" }, 400); }
      if (![input.host_id, input.host_key, input.pairing_hash].every(x => /^[a-f0-9]{64}$/.test(x))) return json({ error: "invalid_registration" }, 400);
      const key = `host:${input.host_id}`;
      const previous = await this.ctx.storage.get(key);
      const key_hash = await hash(input.host_key);
      if (previous && previous.key_hash !== key_hash) return json({ error: "unauthorized" }, 401);
      const host = previous || { key_hash, paired: false, generation: random() };
      if (!host.paired) {
        if (host.pairing_hash) await this.ctx.storage.delete(`pair:${host.pairing_hash}`);
        host.pairing_hash = input.pairing_hash;
        await this.ctx.storage.put(`pair:${input.pairing_hash}`, { host_id: input.host_id, expires: now + 600000 });
      }
      await this.ctx.storage.put(key, host);
      await this.scheduleCleanup();
      return json({ paired: host.paired });
    }
    const hostMatch = /^\/hosts\/([a-f0-9]{64})(\/connect)?$/.exec(url.pathname);
    if (hostMatch) {
      const host_id = hostMatch[1];
      const host = await this.ctx.storage.get(`host:${host_id}`);
      const bearer = request.headers.get("Authorization")?.replace(/^Bearer /, "");
      if (!host || !bearer || await hash(bearer) !== host.key_hash) return json({ error: "unauthorized" }, 401);
      if (request.method === "DELETE" && !hostMatch[2]) {
        await this.ctx.storage.delete(`host:${host_id}`);
        if (host.pairing_hash) await this.ctx.storage.delete(`pair:${host.pairing_hash}`);
        for (const socket of this.ctx.getWebSockets(host_id)) socket.close(1000, "Disconnected");
        return json({ disconnected: true });
      }
      if (hostMatch[2] && request.headers.get("Upgrade")?.toLowerCase() === "websocket") {
        for (const old of this.ctx.getWebSockets(host_id)) old.close(1000, "Connection replaced");
        const pair = new WebSocketPair();
        const [client, server] = Object.values(pair);
        this.ctx.acceptWebSocket(server, [host_id]);
        server.serializeAttachment({ host_id });
        server.send(Relay.connection(host.paired));
        return new Response(null, { status: 101, webSocket: client });
      }
      return json({ error: "method_not_allowed" }, 405);
    }
    if (url.pathname === "/oauth/authorize") {
      if (!await this.rate(request, 30)) return json({ error: "rate_limited" }, 429);
      if (request.method === "GET") {
        const p = Object.fromEntries(url.searchParams);
        const client = await this.ctx.storage.get(`client:${p.client_id}`);
        if (!client || !client.redirect_uris.includes(p.redirect_uri) || p.resource !== resource ||
            p.response_type !== "code" || p.code_challenge_method !== "S256" || !/^[A-Za-z0-9_-]{43}$/.test(p.code_challenge || "") ||
            (p.scope && p.scope !== SCOPE) || !p.state || p.state.length > 4096) return json({ error: "invalid_request" }, 400);
        const transaction = random();
        await this.ctx.storage.put(`authorize:${transaction}`, { ...p, expires: now + 600000 });
        await this.scheduleCleanup();
        const html = `<!doctype html><html><head><meta name="viewport" content="width=device-width"><title>Connect Tyde to ChatGPT</title></head><body><main><h1>Connect your Tyde host</h1><p>In Tyde, open Settings → Integrations → ChatGPT Dots and copy the pairing code.</p><p>Authorizing gives ChatGPT and your dot access to this host's projects and agents. They can start agents that modify files and use your configured providers. Only connect a host you control.</p><form method="post" action="/oauth/authorize"><input type="hidden" name="transaction" value="${escape(transaction)}"><label>Pairing code <input name="pairing_code" type="password" required autocomplete="off" maxlength="64"></label><button type="submit">Authorize ChatGPT</button></form></main></body></html>`;
        return new Response(html, { headers: { "Content-Type": "text/html; charset=utf-8", "Cache-Control": "no-store", "Referrer-Policy": "no-referrer", "X-Content-Type-Options": "nosniff", "Content-Security-Policy": "default-src 'none'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'" } });
      }
      if (request.method === "POST") {
        if (request.headers.get("Origin") !== origin) return json({ error: "invalid_origin" }, 403);
        const form = new URLSearchParams(await body(request));
        const transaction = form.get("transaction");
        const pending = await this.ctx.storage.get(`authorize:${transaction}`);
        if (!pending || pending.expires < now) return json({ error: "authorization_expired" }, 400);
        const pairing_hash = await hash(form.get("pairing_code") || "");
        const pair = await this.ctx.storage.get(`pair:${pairing_hash}`);
        if (!pair || pair.expires < now) return json({ error: "invalid_pairing_code" }, 400);
        const host = await this.ctx.storage.get(`host:${pair.host_id}`);
        if (!host || host.pairing_hash !== pairing_hash) return json({ error: "invalid_pairing_code" }, 400);
        const code = random();
        await this.ctx.storage.transaction(async storage => {
          // Recheck inside the transaction: pairing and authorization codes are single-use.
          if (!await storage.get(`pair:${pairing_hash}`) || !await storage.get(`authorize:${transaction}`)) throw new Error("Pairing already consumed");
          await storage.delete([`pair:${pairing_hash}`, `authorize:${transaction}`]);
          host.paired = true;
          delete host.pairing_hash;
          await storage.put(`host:${pair.host_id}`, host);
          await storage.put(`code:${await hash(code)}`, { ...pending, host_id: pair.host_id, generation: host.generation, expires: now + 60000 });
        });
        for (const socket of this.ctx.getWebSockets(pair.host_id)) socket.send(Relay.connection(true));
        const redirect = new URL(pending.redirect_uri);
        redirect.searchParams.set("code", code);
        redirect.searchParams.set("state", pending.state);
        redirect.searchParams.set("iss", origin);
        return new Response(null, { status: 302, headers: { Location: redirect.href, "Cache-Control": "no-store", "Referrer-Policy": "no-referrer" } });
      }
    }
    if (url.pathname === "/oauth/token" && request.method === "POST") {
      if (!await this.rate(request, 60)) return json({ error: "rate_limited" }, 429);
      const p = Object.fromEntries(new URLSearchParams(await body(request)));
      const key = p.grant_type === "authorization_code" ? `code:${await hash(p.code || "")}` : p.grant_type === "refresh_token" ? `refresh:${await hash(p.refresh_token || "")}` : null;
      if (!key) return json({ error: "unsupported_grant_type" }, 400);
      const grant = await this.ctx.storage.get(key);
      if (!grant || grant.expires < now || grant.client_id !== p.client_id || grant.resource !== p.resource ||
          (p.grant_type === "authorization_code" && (grant.redirect_uri !== p.redirect_uri || !/^[A-Za-z0-9._~-]{43,128}$/.test(p.code_verifier || "") || await pkce(p.code_verifier) !== grant.code_challenge)) ||
          (await this.ctx.storage.get(`host:${grant.host_id}`))?.generation !== grant.generation) return json({ error: "invalid_grant" }, 400);
      const access_token = random();
      const refresh_token = random();
      await this.ctx.storage.transaction(async storage => {
        if (!await storage.get(key)) throw new Error("Grant already consumed");
        await storage.delete(key);
        const record = { host_id: grant.host_id, generation: grant.generation, client_id: grant.client_id, resource, scope: SCOPE };
        await storage.put(`token:${await hash(access_token)}`, { ...record, expires: now + 3600000 });
        await storage.put(`refresh:${await hash(refresh_token)}`, { ...record, expires: now + 2592000000 });
      });
      await this.scheduleCleanup();
      return json({ access_token, refresh_token, token_type: "Bearer", expires_in: 3600, scope: SCOPE });
    }
    if (url.pathname === "/mcp") {
      const bearer = request.headers.get("Authorization")?.match(/^Bearer ([a-f0-9]{64})$/)?.[1];
      const token = bearer ? await this.ctx.storage.get(`token:${await hash(bearer)}`) : null;
      if (!token || token.expires < now || token.resource !== resource || token.scope !== SCOPE || (await this.ctx.storage.get(`host:${token.host_id}`))?.generation !== token.generation) {
        return json({ error: "unauthorized" }, 401, { "WWW-Authenticate": `Bearer resource_metadata="${origin}/.well-known/oauth-protected-resource", scope="${SCOPE}"` });
      }
      if (request.method !== "POST") return json({ error: "method_not_allowed" }, 405);
      const sockets = this.ctx.getWebSockets(token.host_id);
      if (sockets.length !== 1) return json({ error: "host_offline" }, 503);
      if (this.pending.size >= 64) return json({ error: "busy" }, 503);
      const transport_id = random();
      const payload = await body(request);
      return await new Promise(resolve => {
        const timer = setTimeout(() => { this.pending.delete(transport_id); resolve(json({ error: "host_timeout" }, 504)); }, 25000);
        this.pending.set(transport_id, { host_id: token.host_id, resolve, timer });
        try { sockets[0].send(Relay.request(payload, transport_id)); }
        catch { clearTimeout(timer); this.pending.delete(transport_id); resolve(json({ error: "host_offline" }, 503)); }
      });
    }
    return json({ error: "not_found" }, 404);
  }

  webSocketMessage(socket, message) {
    if (typeof message !== "string" || message.length > 2097152) { socket.close(1009, "Invalid transport response"); return; }
    let frame;
    try { frame = Relay.decode(message); } catch { socket.close(1003, "Invalid transport response"); return; }
    const pending = this.pending.get(frame.transport_id);
    if (!pending || pending.host_id !== socket.deserializeAttachment().host_id) return;
    if (!Relay.isResponse(frame) || frame.status < 200 || frame.status > 599) {
      socket.close(1003, "Invalid transport response"); return;
    }
    clearTimeout(pending.timer);
    this.pending.delete(frame.transport_id);
    pending.resolve(new Response(frame.body || null, { status: frame.status, headers: { "Content-Type": "application/json", "Cache-Control": "no-store" } }));
  }

  webSocketClose(socket) { this.disconnected(socket); }
  webSocketError(socket) { this.disconnected(socket); }
  disconnected(socket) {
    const host_id = socket.deserializeAttachment().host_id;
    for (const [id, pending] of this.pending) if (pending.host_id === host_id) {
      clearTimeout(pending.timer); this.pending.delete(id); pending.resolve(json({ error: "host_offline" }, 503));
    }
  }

  async rate(request, limit) {
    const ip = request.headers.get("CF-Connecting-IP") || "local";
    const key = `rate:${await hash(ip)}:${new URL(request.url).pathname}:${Math.floor(this.time() / 60000)}`;
    const used = (await this.ctx.storage.get(key)) || 0;
    if (used >= limit) return false;
    await this.ctx.storage.put(key, used + 1);
    await this.scheduleCleanup();
    return true;
  }

  time() {
    if (this.env.TEST_TIME_OFFSET === undefined) return Date.now();
    const offset = Number(this.env.TEST_TIME_OFFSET);
    if (!Number.isSafeInteger(offset) || offset < 0) throw new Error("Invalid test clock offset");
    return Date.now() + offset;
  }

  async scheduleCleanup() {
    if (!await this.ctx.storage.getAlarm()) await this.ctx.storage.setAlarm(Date.now() + 60000);
  }
  async alarm() {
    const now = this.time();
    const entries = await this.ctx.storage.list();
    for (const [key, value] of entries) {
      if ((value?.expires && value.expires < now) || (key.startsWith("rate:") && Number(key.split(":").at(-1)) < Math.floor(now / 60000) - 1)) await this.ctx.storage.delete(key);
    }
    if (entries.size) await this.ctx.storage.setAlarm(now + 60000);
  }
}
