import { resolveNativeHost, resolveNativeOffer, nativeSignIn, NativeBootstrapError, nativeDescriptorIsLive } from './native-service.js';
import { artifactSetSha256, nativeBridgeCompatible } from './manifest-policy.js';

// Discovery chooses identity; executable selection and delivery stay in loader.js.
export async function startNativeBootstrap(hooks) {
  const bridge = window.__tydeNativeHost;
  if (bridge.bootstrapVersion !== 1 || bridge.version !== 6) throw new Error('Native bootstrap ABI is incompatible');
  await bridge.post('presentation', JSON.stringify({version:bridge.version, client_ready:false, workspace:false, tab:'home', route:'root',
    title:'Tyde', subtitle:null, chat_actions:false, modal:false, theme:'dark', sidebar_available:false, sidebar_open:false}));

  let selected = hooks.selected();
  let allowed = false;
  let pairingAllowed = false;
  let pairingActive = false;
  let clientStarted = false;
  let pending = false;
  let booted = null;
  let bootedHost = selected;
  let bootedDescriptor = null;
  let generation = 0;
  let firstPairAdmission = false;
  let pairingDescriptor = null;
  let pairingStarted = false;
  const initialHosts = new Set((await hooks.readHosts()).map(record => record.localHostId));
  const requests = new Map();
  const pairKey = 'tyde.native.pending-pair.v1';
  const forgetKey = 'tyde.native.pending-forget.v1';
  const fail = () => ({ status: 'unavailable', reason: 'bootstrap' });
  const failure = error => {
    const reason = error?.code === 'transport' ? 'bootstrap_transport' : error?.code;
    return {status:'unavailable',reason:['authentication_required','pass_required','pairing_revoked','native_version','bootstrap_baseline','bootstrap_transport','host_unavailable','pairing_secret_missing','credential_storage_unavailable'].includes(reason) ? reason : 'bootstrap'};
  };
  const handoffPairing = uri => {
    sessionStorage.setItem(pairKey, uri);
    pending = true;
    location.reload();
    return {status:'reloading'};
  };
  const reload = () => {
    if (allowed && pending) location.reload();
  };
  const current = async host => {
    if (!requests.has(host)) {
      const request = (async () => {
        const record = (await hooks.readHosts()).find(item => item.localHostId === host);
        if (!record) throw new Error('The selected pairing is unavailable');
        return resolveNativeHost(record);
      })();
      requests.set(host, request);
      request.finally(() => { if (requests.get(host) === request) requests.delete(host); }).catch(() => {});
    }
    return requests.get(host);
  };
  const target = async descriptor => {
    let manifest;
    try { manifest = await hooks.fetchManifest(); }
    catch { throw new NativeBootstrapError('transport', 'The trusted client publisher is temporarily unavailable.'); }
    const result = hooks.resolveBootTarget(descriptor.client_release_version, manifest);
    if (!result.ok) throw new NativeBootstrapError('bootstrap', 'The exact host client is unavailable or blocked.');
    if (!hooks.checkProtocolCompatibility(descriptor.protocol_version, result).ok || result.transportProtocolVersion !== descriptor.transport_protocol_version ||
        await artifactSetSha256(result.artifacts) !== descriptor.artifact_set_sha256) throw new NativeBootstrapError('bootstrap', 'The published client does not match the current host.');
    if (!nativeBridgeCompatible(result, bridge.version)) throw new NativeBootstrapError('native_version', 'Update the app: this client requires a different native bridge.');
    return { ...result, artifactSetSha256: descriptor.artifact_set_sha256 };
  };
  const same = descriptor => booted && descriptor.client_release_version === booted.version &&
    descriptor.protocol_version === booted.protocolVersion && descriptor.transport_protocol_version === booted.transportProtocolVersion && descriptor.artifact_set_sha256 === booted.artifactSetSha256;
  const change = () => {
    pending = true;
    reload();
    return { status: 'unavailable', reason: 'host_changed' };
  };
  const runtime = {
    recoverHosts() {
      if (clientStarted && !pairingAllowed) { hooks.error('Finish or save your current work before switching hosts.'); return; }
      sessionStorage.removeItem(pairKey); sessionStorage.removeItem(forgetKey); hooks.clearPairing();
      localStorage.removeItem('tyde.selected-host.v1'); location.reload();
    },
    async forgetHost(host) {
      if (!clientStarted || !pairingAllowed || pairingActive) return {status:'unavailable',reason:'host_changed'};
      const ticket = generation;
      if (!(await hooks.readHosts()).some(record => record.localHostId === host)) return fail();
      if (ticket !== generation || !pairingAllowed || pairingActive) return {status:'unavailable',reason:'cancelled'};
      // Only a new bootstrap document owns cross-host credentials.
      sessionStorage.setItem(forgetKey, host);
      location.reload();
      return {status:'reloading'};
    },
    async pairingSaved(host) {
      if (initialHosts.has(host) || !pairingDescriptor) return fail();
      const record = (await hooks.readHosts()).find(item => item.localHostId === host);
      if (!record) return fail();
      // The loaded client has just redeemed the freshly resolved offer. Its first
      // authenticated connection confirms that target; later connections resolve the pairing.
      const keys = [record.pskKeychainKeyId, record.managed?.deviceSecretKeyId, record.direct?.deviceTokenKeyId].filter(Boolean);
      await bridge.scopeSecrets(keys);
      bootedDescriptor = pairingDescriptor; pairingDescriptor = null;
      bootedHost = host; selected = host; firstPairAdmission = true;
      sessionStorage.removeItem(pairKey);
      hooks.clearPairing();
      return {status:'matching'};
    },
    confirmWelcome(descriptor) {
      if (!nativeDescriptorIsLive(bootedDescriptor) || !Object.keys(bootedDescriptor).every(key => bootedDescriptor[key] === descriptor?.[key])) return {status:'unavailable',reason:'protocol'};
      firstPairAdmission = false;
      return {status:'matching'};
    },
    ownsHost(host) { return host === bootedHost && host === selected; },
    setReloadAllowed(value, pairing, handoffComplete, pairingSafe) {
      clientStarted = true;
      if (pairingActive && !pairing) {
        generation++;
        if (bootedHost !== null) {
          sessionStorage.removeItem(pairKey); hooks.clearPairing();
          pairingDescriptor = null;
        }
      }
      pairingActive = pairing === true;
      pairingAllowed = pairingSafe === true && handoffComplete === true;
      allowed = value === true && handoffComplete === true;
      if (allowed && !pending && !pairing && bootedHost === null && pairingDescriptor && pairingStarted) {
        sessionStorage.removeItem(pairKey);
        hooks.clearPairing();
        location.reload();
        return;
      }
      reload();
    },
    select(host) {
      if (bootedHost === null && pairingDescriptor) return;
      if (selected === host) return;
      selected = host;
      generation++;
      if (host && host !== bootedHost) change();
    },
    async switchTarget(host) {
      if (host !== selected) throw new NativeBootstrapError('bootstrap', 'Host selection changed.');
      const ticket = generation;
      const descriptor = await current(host);
      await target(descriptor);
      if (ticket !== generation || host !== selected) throw new NativeBootstrapError('cancelled', 'Host selection changed.');
      return descriptor;
    },
    async admitHost(host) {
      if (host !== selected) return { status: 'unavailable', reason: 'cancelled' };
      if (host !== bootedHost) return change();
      if (firstPairAdmission && nativeDescriptorIsLive(bootedDescriptor)) return {status:'matching'};
      const ticket = generation;
      try {
        const descriptor = await current(host);
        if (host !== selected || ticket !== generation) return { status: 'unavailable', reason: 'cancelled' };
        await target(descriptor);
        if (host !== selected || ticket !== generation) return {status:'unavailable',reason:'cancelled'};
        if (same(descriptor)) { bootedDescriptor = descriptor; firstPairAdmission = false; return { status: 'matching' }; }
        return change();
      } catch (error) {
        return failure(error);
      }
    },
    async preparePairing(uri) {
      const initialOffer = bootedHost === null && !pairingStarted && uri === sessionStorage.getItem(pairKey);
      if ((!pairingAllowed || !pairingActive) && !initialOffer) return {status:'unavailable',reason:'host_changed'};
      const ticket = generation;
      try {
        const descriptor = await resolveNativeOffer(uri);
        if (ticket !== generation || (!initialOffer && (!pairingActive || !pairingAllowed))) return {status:'unavailable',reason:'cancelled'};
        await target(descriptor);
        if (ticket !== generation || (!initialOffer && (!pairingActive || !pairingAllowed))) return {status:'unavailable',reason:'cancelled'};
        if (same(descriptor)) { pairingDescriptor = descriptor; pairingStarted = true; return { status: 'matching' }; }
        // A successful explicit Continue/scan, with no unsaved UI owners,
        // authorizes this one handoff independently of background host upgrades.
        return handoffPairing(uri);
      } catch (error) {
        if (ticket !== generation || (!initialOffer && (!pairingActive || !pairingAllowed))) return {status:'unavailable',reason:'cancelled'};
        // Only an explicitly safe Continue/scan reaches here. Bootstrap owns
        // sign-in before a different host's full client can be selected.
        if (error?.code === 'authentication_required') return handoffPairing(uri);
        return failure(error);
      }
    },
  };
  hooks.installRuntime(runtime);
  const boot = async (descriptor, host, uri, ticket = generation) => {
    const resolved = await target(descriptor);
    if (ticket !== generation) return {status:'unavailable',reason:'cancelled'};
    const record = host ? (await hooks.readHosts()).find(item => item.localHostId === host) : null;
    if (ticket !== generation) return {status:'unavailable',reason:'cancelled'};
    const keys = record ? [record.pskKeychainKeyId, record.managed?.deviceSecretKeyId, record.direct?.deviceTokenKeyId].filter(Boolean) : [];
    // Commit one document owner before narrowing its native secret scope.
    booted = resolved;
    hooks.show('loading');
    await bridge.scopeSecrets(keys);
    if (ticket !== generation) return {status:'unavailable',reason:'cancelled'};
    bootedDescriptor = descriptor;
    if (uri) pairingDescriptor = descriptor;
    bootedHost = host;
    await hooks.boot({ ...resolved, runtime, nativeBeforeExecute: async () => {
      if (ticket !== generation) throw new NativeBootstrapError('cancelled', 'Host selection changed.');
      const fresh = host ? await current(host) : await resolveNativeOffer(uri);
      if (ticket !== generation) throw new NativeBootstrapError('cancelled', 'Host selection changed.');
      if (!same(fresh)) throw new NativeBootstrapError('bootstrap', 'The host changed while its client was loading. Try again.');
      bootedDescriptor = fresh;
    } }, host, uri);
  };
  const offer = async uri => {
    if (booted) return {status:'unavailable',reason:'host_changed'};
    const ticket = ++generation;
    sessionStorage.setItem(pairKey, uri);
    try {
      const descriptor = await resolveNativeOffer(uri);
      if (ticket !== generation) return {status:'unavailable',reason:'cancelled'};
      return await boot(descriptor, null, uri, ticket);
    } catch (error) {
      if (ticket !== generation) return {status:'unavailable',reason:'cancelled'};
      showFailure(error);
      return failure(error);
    }
  };
  const showFailure = error => {
    // Only local contextual messages, never service response bodies/URLs/secrets.
    hooks.error(error instanceof NativeBootstrapError ? error.message : 'The current host client is unavailable.');
    const view = document.getElementById('view-error');
    if (error?.code === 'authentication_required') {
      for (const provider of ['apple', 'google']) {
        const button = document.createElement('button');
        button.type = 'button'; button.className = 'btn primary';
        button.textContent = provider === 'apple' ? 'Continue with Apple' : 'Continue with Google';
        button.addEventListener('click', async () => {
          button.disabled = true;
          try { await nativeSignIn(provider); location.reload(); }
          catch { hooks.error('Sign-in could not complete.'); }
          finally { button.disabled = false; }
        });
        view.querySelector('.card').append(button);
      }
    }
  };
  const forgotten = sessionStorage.getItem(forgetKey);
  if (forgotten) {
    try {
      const record = (await hooks.readHosts()).find(item => item.localHostId === forgotten);
      if (!record) throw new Error('Pairing removal lost its record');
      for (const key of [record.pskKeychainKeyId, record.managed?.deviceSecretKeyId, record.direct?.deviceTokenKeyId].filter(Boolean)) {
        await bridge.post('secret_delete', JSON.stringify({key}));
      }
      await hooks.removeHost(record);
      sessionStorage.removeItem(forgetKey);
      if (selected === forgotten) {
        localStorage.removeItem('tyde.selected-host.v1');
        selected = null;
      }
    } catch {
      hooks.error('Pairing cleanup failed. The saved host is retained; try again to finish removing it.');
      return runtime;
    }
  }
  const uri = hooks.pendingPairing || sessionStorage.getItem(pairKey);
  if (uri) await offer(uri);
  else if (selected) {
    try { await boot(await current(selected), selected, null); }
    catch (error) { showFailure(error); }
  } else {
    hooks.show('pair');
    hooks.scan(offer);
    const records = await hooks.readHosts();
    const form = document.getElementById('paste-form');
    for (const record of records) {
      const button = document.createElement('button');
      button.type = 'button'; button.className = 'btn primary';
      button.textContent = [record.hostLabel, record.direct?.nativeOrigin, record.credentialFingerprint].filter(Boolean).join(' · ');
      button.addEventListener('click', () => {
        localStorage.setItem('tyde.selected-host.v1', record.localHostId);
        location.reload();
      });
      form.before(button);
    }
    form.addEventListener('submit', event => {
      event.preventDefault();
      void offer(document.getElementById('paste-input').value);
    });
  }
  return runtime;
}
