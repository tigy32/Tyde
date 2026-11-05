import { extractPairingUri, parsePairingUri, validateReleaseVersion, PAIRING_V2_PREFIX, PAIRING_V3_PREFIX } from './pairing.js';

const prefix = '/api/tyde/mobile/v1';
const encoder = new TextEncoder();
const bridge = () => window.__tydeNativeHost;
const starts = new WeakMap();
const leases = new WeakMap();
export const nativeDescriptorIsLive = descriptor => leases.has(descriptor) && performance.now() < leases.get(descriptor);
const post = async (endpoint, body) => bridge().post(endpoint, JSON.stringify(body));

async function pairingSecret(key) {
  let secret;
  try { secret = await post('secret_read', {key}); }
  catch { throw new NativeBootstrapError('credential_storage_unavailable', 'The app could not access credential storage. Unlock the device and retry.'); }
  if (typeof secret !== 'string' || !secret) throw new NativeBootstrapError('pairing_secret_missing', 'The pairing credential is missing on this device. Re-pair from the host’s Settings → Mobile.');
  return secret;
}
const base64url = bytes => btoa(String.fromCharCode(...new Uint8Array(bytes))).replaceAll('+', '-').replaceAll('/', '_').replace(/=+$/, '');

export class NativeBootstrapError extends Error {
  constructor(code, message) { super(message); this.code = code; }
}

export function nativeBootstrapFailure(error) {
  const reason = error?.code === 'transport' ? 'bootstrap_transport' : error?.code;
  return {status:'unavailable',reason:['authentication_required','pass_required','pairing_revoked','native_version','bootstrap_baseline','bootstrap_transport','host_unavailable','pairing_secret_missing','credential_storage_unavailable'].includes(reason) ? reason : 'bootstrap'};
}

async function request(path, body, headers = {}, direct = false) {
  const started = performance.now();
  let response;
  try {
    response = await fetch(path, { method: 'POST', credentials: 'include', cache: 'no-store', redirect: 'error',
      headers: { 'content-type': 'application/json', ...headers }, body: JSON.stringify(body) });
  } catch { throw new NativeBootstrapError('transport', 'The host discovery connection is unavailable.'); }
  if (response.status === 502 || response.status === 503 || response.status === 429) throw new NativeBootstrapError('transport', 'Host discovery is temporarily unavailable.');
  if (!response.ok) {
    if (response.status === 401) throw new NativeBootstrapError('authentication_required', 'Sign in to connect to your host.');
    if (direct && response.status === 404) throw new NativeBootstrapError('bootstrap_baseline', 'Update Tyde host to support native client bootstrap.');
    const error = await response.json();
    const messages = {
      native_abi_incompatible: 'Update the app: this host client requires a different native bridge.',
      bootstrap_unknown: 'Update Tyde host to support native client bootstrap.',
      bootstrap_stale: 'The host’s mobile access registration is unavailable.',
      pairing_revoked: 'This pairing has been revoked.',
      revoked_device: 'This pairing has been revoked.',
      pairing_rejected: 'This pairing offer has expired or its host client changed.',
      offer_expired: 'This pairing offer has expired.',
    };
    const code = direct ? error?.code : error?.error?.code;
    const reasons = {native_abi_incompatible:'native_version',bootstrap_unknown:'bootstrap_baseline',bootstrap_stale:'host_unavailable',
      pairing_revoked:'pairing_revoked',revoked_device:'pairing_revoked',unknown_device:'pairing_revoked',not_found:'pairing_revoked',forbidden:'pairing_revoked',pass_required:'pass_required'};
    throw new NativeBootstrapError(reasons[code] || 'bootstrap', messages[code] || 'The current host client is unavailable.');
  }
  const value = await response.json();
  if (value && typeof value === 'object') starts.set(value, started);
  return value;
}

function closed(value, keys) {
  return value && typeof value === 'object' && !Array.isArray(value) &&
    Object.keys(value).length === keys.length && keys.every(key => Object.hasOwn(value, key));
}

function resolved(value, kind, id) {
  const d = value?.descriptor;
  const positive = n => Number.isInteger(n) && n > 0 && n <= 0xffffffff;
  if (positive(d?.native_bridge_abi) && d.native_bridge_abi !== bridge().version) throw new NativeBootstrapError('native_version', 'Update the app: this host client requires a different native bridge.');
  const release = s => typeof s === 'string' && s.length <= 128 && /^[\x21-\x7e]+$/.test(s) && validateReleaseVersion(s) === s;
  if (!closed(value, ['binding','descriptor','generation','registered_at_ms','expires_at_ms','resolved_at_ms']) ||
      !closed(value.binding, ['kind','id']) || value.binding.kind !== kind || value.binding.id !== id ||
      !closed(d, ['bootstrap_version','host_release_version','protocol_version','transport_protocol_version','client_release_version','artifact_set_sha256','native_bridge_abi']) ||
      d.bootstrap_version !== 1 || !release(d.host_release_version) || !release(d.client_release_version) ||
      !positive(d.protocol_version) || !positive(d.transport_protocol_version) || d.native_bridge_abi !== bridge().version ||
      !/^[0-9a-f]{64}$/.test(d.artifact_set_sha256) ||
      ![value.generation,value.registered_at_ms,value.expires_at_ms,value.resolved_at_ms].every(Number.isSafeInteger) ||
      value.generation < 1 || value.registered_at_ms < 0 || value.resolved_at_ms < value.registered_at_ms ||
      value.expires_at_ms <= value.resolved_at_ms || value.expires_at_ms - value.registered_at_ms > 120000) {
    throw new NativeBootstrapError('bootstrap', 'The host bootstrap response is invalid or incompatible.');
  }
  leases.set(d, starts.get(value) + value.expires_at_ms - value.resolved_at_ms);
  if (!nativeDescriptorIsLive(d)) throw new NativeBootstrapError('bootstrap', 'The host client registration expired during discovery.');
  return d;
}

export async function resolveNativeHost(record) {
  if (record.direct) {
    const token = await pairingSecret(record.direct.deviceTokenKeyId);
    const origin = new URL(record.direct.nativeOrigin);
    if (origin.protocol !== 'https:' || origin.username || origin.password || origin.pathname !== '/' || origin.search || origin.hash) {
      throw new NativeBootstrapError('bootstrap', 'The paired host origin is invalid.');
    }
    return resolved(await request(new URL('/tyde/bootstrap', origin), {native_bridge_abi:bridge().version},
      {authorization:'Bearer '+token},true), 'device', record.direct.deviceId);
  }
  const managed = record.managed;
  if (!managed || !/^[A-Za-z0-9_-]+$/.test(managed.pairingId)) throw new NativeBootstrapError('bootstrap', 'The managed pairing is unavailable.');
  const body = { native_bridge_abi: bridge().version };
  const path = `${prefix}/pairings/${managed.pairingId}/bootstrap/resolve`;
  const secret = await pairingSecret(managed.deviceSecretKeyId);
  const nonce = crypto.randomUUID(), timestamp = Date.now();
  const bodyHash = base64url(await crypto.subtle.digest('SHA-256', encoder.encode(JSON.stringify(body))));
  const canonical = ['TYCODE-PAIRING-HMAC-V1','POST',path,bodyHash,nonce,String(timestamp),managed.pairingId,'mobile'].join('\n');
  const key = await crypto.subtle.importKey('raw', encoder.encode(secret), {name:'HMAC',hash:'SHA-256'}, false, ['sign']);
  const signature = base64url(await crypto.subtle.sign('HMAC', key, encoder.encode(canonical)));
  const authorization = `v1;role=mobile;nonce=${nonce};timestamp_ms=${timestamp};signature=${signature}`;
  return resolved(await request(path, body, {'x-tycode-pairing-auth':authorization}), 'pairing', managed.pairingId);
}

export async function resolveNativeOffer(uri) {
  const inner = extractPairingUri(uri);
  const parsed = parsePairingUri(inner, true);
  if (!parsed.offerId || !parsed.offerSecret) throw new NativeBootstrapError('bootstrap', 'Update Tyde host to support native bootstrap.');
  const body = { native_bridge_abi: bridge().version, offer_secret: parsed.offerSecret };
  if (inner.startsWith(PAIRING_V3_PREFIX)) {
    const origin = new URL(uri);
    if (origin.protocol !== 'https:' || origin.username || origin.password || origin.pathname !== '/tyde/' || origin.search) {
      throw new NativeBootstrapError('bootstrap', 'Direct pairing requires the complete HTTPS host link.');
    }
    body.offer_id = parsed.offerId;
    return resolved(await request(new URL('/tyde/bootstrap', origin), body,{},true), 'offer', parsed.offerId);
  }
  if (!inner.startsWith(PAIRING_V2_PREFIX) || !/^[A-Za-z0-9_-]+$/.test(parsed.offerId)) throw new NativeBootstrapError('bootstrap', 'This pairing code cannot bootstrap a client.');
  return resolved(await request(`${prefix}/host/offers/${parsed.offerId}/bootstrap/resolve`, body), 'offer', parsed.offerId);
}

export async function nativeSignIn(provider) {
  const callback = new URL(await post('authenticate', {provider}));
  if (callback.protocol !== 'tyde-mobile:' || callback.host !== 'auth' || callback.pathname !== '/callback' || callback.username || callback.password || callback.port) {
    throw new NativeBootstrapError('authentication', 'Native sign-in callback is invalid.');
  }
  const parameters = new URLSearchParams([...callback.searchParams, ...new URLSearchParams(callback.hash.slice(1))]);
  const handoffs = parameters.getAll('handoffCode');
  const handoff = handoffs.length === 1 ? handoffs[0] : null;
  if (!handoff) throw new NativeBootstrapError('authentication', 'Native sign-in did not complete.');
  const session = await request(`${prefix}/auth/session`, { handoff_code: handoff,
    client: {kind:'native_bootstrap_ios',release_version:bridge().shellRelease,protocol_version:bridge().bootstrapVersion} });
  if (session.authenticated !== true) throw new NativeBootstrapError('authentication', 'Native sign-in was rejected.');
}
