import { test } from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { readFile, writeFile, mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { execFile } from "node:child_process";
import { chromium } from "playwright";
import { once } from "node:events";

// Real browser + HTTP + CacheStorage + IndexedDB + worker lifecycle. The tiny
// immutable modules are executable loader fixtures, not fake agent backends.
test("installed storage migrates once, then follows exact targets without losing pairing", async t => {
  assert.ok(process.env.CHROME, "dev.sh must provision Chrome");
  const root = new URL("../", import.meta.url);
  const old = "1.0.0", capable = "1.0.1", upgraded = "1.0.2";
  const wasm = Buffer.from([0,97,115,109,1,0,0,0]);
  const code = version => Buffer.from(`export default async function() { document.getElementById('app-root').textContent = 'Client ${version}'; }`);
  const directory = await mkdtemp(join(tmpdir(), "tyde-loader-flow-"));
  t.after(() => rm(directory, {recursive:true,force:true}));
  const protocolSource = join(directory, "types.rs");
  const manifestPath = join(directory, "manifest.json");
  await writeFile(protocolSource, "pub const PROTOCOL_VERSION: u32 = 66;");
  const marker = (await readFile(new URL("../../../mobile-frontend/index.html", import.meta.url),"utf8"))
    .match(/<meta name="tyde-follow-selected-host" content="1" \/>/)?.[0];
  assert.ok(marker,"the built client declares its capability");
  for (const version of [old,capable,upgraded]) {
    await writeFile(protocolSource, `pub const PROTOCOL_VERSION: u32 = ${version === upgraded ? 67 : 66};`);
    const dist = join(directory,version);await mkdir(dist);
    await writeFile(join(dist,"app.js"),code(version));
    await writeFile(join(dist,"app_bg.wasm"),wasm);
    await writeFile(join(dist,"index.html"),'<!doctype html>'+(version!==old?marker:"")+'<script type="module">import init from "/tyde/v'+version+'/app.js";</script>');
    await promisify(execFile)(process.execPath,[new URL("../../deploy/generate-manifest.mjs",import.meta.url).pathname,
      "--dist",dist,"--version",version,"--manifest",manifestPath,"--protocol-source",protocolSource]);
  }
  const generated = JSON.parse(await readFile(manifestPath,"utf8"));
  assert.equal(generated.versions[old].followsSelectedHost,undefined,"historical artifact gains no capability during backfill");
  assert.equal(generated.versions[capable].followsSelectedHost,1);
  let published = false, legacy = true, offline = false, tamper = false, blocked = false, scannerOnly = false;
  let legacyWorkerActive = true;
  let servedClient = null, directPolicy = {}, directEntry = {};
  let manifestRequests = 0;
  const manifest = () => {
    const value = structuredClone(generated);
    if (scannerOnly) { value.versions = {}; return value; }
    value.blocked = blocked ? [old] : [];
    if (!published) delete value.versions[capable].followsSelectedHost;
    if (servedClient !== null) {
      value.versions = { [servedClient]: { ...value.versions[servedClient], ...directEntry } };
      value.minSupported = servedClient;
      value.servedClientRelease = servedClient;
      Object.assign(value, directPolicy);
    } else {
      delete value.versions[upgraded];
    }
    return value;
  };
  const setup = `const done=arguments[arguments.length-1];new Promise((resolve,reject) => {
    const open=indexedDB.open('tyde-mobile',1);
    open.onupgradeneeded=()=>{open.result.createObjectStore('paired_hosts');open.result.createObjectStore('psk');};
    open.onerror=()=>reject(new Error('storage setup failed'));
    open.onsuccess=()=>{const db=open.result;const tx=db.transaction(['paired_hosts','psk'],'readwrite');
      tx.objectStore('paired_hosts').put(JSON.stringify([{localHostId:'retained-host'},{localHostId:'second-host'},{localHostId:'third-host'}]),'all');
      tx.objectStore('psk').put('fixture-only-key','retained-key');
      tx.oncomplete=()=>{db.close();localStorage.setItem('tyde.loader.version','${old}');resolve(true);};};
  }).then(done,()=>done(false));`;
  // Historical root-loader contract: remembered pin, no Welcome-release API.
  // Same immutable targets, origin, worker scope and browser stores throughout.
  const legacyLoader = `import {resolveBootTarget} from './manifest-policy.js';
    import {verifyArtifacts} from './integrity.js';
    navigator.serviceWorker.register('./sw.js',{scope:'./'});
    const manifest=await (await fetch('./manifest.json',{cache:'no-store'})).json();
    const target=resolveBootTarget(localStorage.getItem('tyde.loader.version'),manifest);
    const checked=await verifyArtifacts(target.artifacts,{cache:await caches.open('tyde-bundle-v1')});
    if(!checked.ok) throw new Error('fixture integrity failure');
    await (await import(target.entry)).default();
    document.getElementById('loader-shell').hidden=true;`;
  const decoderPath = (await readFile(new URL("index.html", root), "utf8"))
    .match(/<script src="\.\/(decoder\/jsqr[^"/]*\.js)"/)[1];
  const decoderBytes = await readFile(new URL(decoderPath, root));
  const legacyWorker = await readFile(new URL("test/fixtures/pre-cache-classifier-sw.js", root));
  const assets = new Set(["index.html", "loader.js", "native-bootstrap.js", "native-service.js", "loader.css", "sw.js", "mobile-service-config.js", "cbor.js", "pairing.js", "pairing-ui.js", "styles.js", "manifest-policy.js", "integrity.js", "manifest.webmanifest", "decoder/jsqr-5f0b827026fedad24be5147cc65e8217e06a643b3967206e91c9d1332e1b3101.js", "icons/icon.svg"]);
  const server = createServer(async (req, res) => {
    try {
      const path = new URL(req.url, "http://fixture.invalid").pathname;
      res.setHeader("Cache-Control", "no-store");
      if (path === "/setup") { res.setHeader("Content-Type","text/html"); res.end('<!doctype html><title>Loader storage setup</title>'); return; }
      if (path === "/tyde/manifest.json") {
        manifestRequests++;
        res.setHeader("Content-Type","application/json");
        res.statusCode = offline ? 503 : 200;
        res.end(JSON.stringify(manifest())); return;
      }
      const artifact = /^\/tyde\/v(1\.0\.[012])\/(app\.js|app_bg\.wasm|index\.html)$/.exec(path);
      if (artifact) {
        const [, version, file] = artifact;
        if (servedClient !== null && version !== servedClient) { res.statusCode=404;res.end();return; }
        res.setHeader("Content-Type", file.endsWith(".js") ? "text/javascript" : file.endsWith(".wasm") ? "application/wasm" : "text/html");
        res.end(file === "index.html" ? "<!doctype html>" : file === "app.js" ? code(version) : tamper ? Buffer.from("tampered") : wasm);
        return;
      }
      const asset = path === "/tyde/" ? "index.html" : path.slice("/tyde/".length);
      if (asset === "vendor/jsqr.js") {
        res.setHeader("Content-Type", "text/javascript"); res.end(decoderBytes); return;
      }
      if (!path.startsWith("/tyde/") || !assets.has(asset)) { res.statusCode=404;res.end();return; }
      let body = await readFile(new URL(asset, root));
      if (legacy && asset === "loader.js") body = Buffer.from(legacyLoader);
      if (legacyWorkerActive && asset === "sw.js") body = legacyWorker;
      res.setHeader("Content-Type", asset.endsWith(".js") ? "text/javascript" : asset.endsWith(".css") ? "text/css" : asset.endsWith(".svg") ? "image/svg+xml" : asset.endsWith(".html") ? "text/html" : "application/json");
      res.end(body);
    } catch { res.statusCode=500;res.end("fixture failure"); }
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const origin = `http://127.0.0.1:${server.address().port}`;
  let browser;
  try {
    // Use the pinned pipe transport: ChromeDriver's two-family port=0 bind
    // can select an IPv6 port already occupied by an unrelated IPv4 listener.
    console.info("loader browser phase: launch");
    browser = await chromium.launch({
      executablePath: process.env.CHROME,
      headless: true,
      args: ["--no-sandbox", "--disable-dev-shm-usage"],
      timeout: 0,
    });
    console.info("loader browser phase: launched");
    const page = await browser.newPage();
    console.info("loader browser phase: page created");
    page.setDefaultTimeout(0);
    page.setDefaultNavigationTimeout(0);
    let command = 0;
    const run = async (kind, operation) => {
      const id = ++command;
      console.info(`loader browser command ${id}: ${kind} start`);
      const result = await operation();
      console.info(`loader browser command ${id}: ${kind} complete`);
      return result;
    };
    const evaluate = script => run("evaluate", () => page.evaluate(`(function(){${script}})()`));
    const evaluateAsync = script => run("async storage/worker", () => page.evaluate(`new Promise(resolve => { (function(){${script}})(resolve); })`));
    const navigate = path => run("navigate", () => page.goto(origin + path, {waitUntil: "load"}));
    const waitFor = text => run("rendered state", () => page.waitForFunction(
      expected => document.body !== null && document.body.textContent.includes(expected), text,
    ).then(handle => handle.jsonValue()));
    await navigate("/setup");
    assert.deepEqual(await evaluate("return [location.origin,location.pathname,document.title];"),
      [origin,"/setup","Loader storage setup"],"storage setup must execute in the fixture document");
    // The retained failure had no setupReady after navigation. Initialize and
    // await the real storage transaction in one command, not a script-load side effect.
    assert.equal(await evaluateAsync(setup),true,"storage transaction must commit before loader navigation");
    await navigate("/tyde/");
    assert.equal(await waitFor(`Client ${old}`),true);
    await evaluateAsync("const done=arguments[arguments.length-1]; navigator.serviceWorker.ready.then(()=>{if(navigator.serviceWorker.controller)done(true);else navigator.serviceWorker.addEventListener('controllerchange',()=>done(true),{once:true});});");
    await t.test("current decoder loads on the first native document under the installed old worker", async () => {
      assert.equal(await page.evaluate(() => navigator.serviceWorker.controller !== null), true);
      legacy = false;
      await navigate("/tyde/?native=1");
      assert.equal(await page.evaluate(() => typeof window.jsQR), "function",
        "an installed old worker must not reject the new decoder before worker activation");
    });
    legacy = false;
    legacyWorkerActive = false;
    // Uploading a new loader before the capable manifest cannot consume migration.
    await navigate("/tyde/");
    assert.equal(await waitFor(`Client ${old}`),true);
    assert.equal(await evaluate("return localStorage.getItem('tyde.loader.follow-host.v1')"),null);
    await evaluate(`document.getElementById('app-root').textContent='';window.dispatchEvent(new CustomEvent('tyde:repair-version',{detail:'${old}'}));return true;`);
    assert.equal(await waitFor(`Client ${old}`),true);
    assert.equal(await evaluate("return document.getElementById('app-root').textContent"),`Client ${old}`, "resume does not replace running immutable code");
    assert.equal(await evaluate("return JSON.parse(localStorage.getItem('tyde.loader.host-target.v1')).version"),old,"actual repair reload completed an exact boot");
    await t.test("legacy repair before publication cannot consume capability migration", async () => {
      assert.equal(await evaluate("return localStorage.getItem('tyde.loader.follow-host.v1')"),null,
        "incapable exact repair must remain eligible for migration");
      published = true;
      await navigate("/tyde/");
      assert.equal(await waitFor(`Client ${capable}`),true,"legacy repair pin must yield to first capable bootstrap");
    });
    // Isolate subsequent boundary scenarios even when the migration regression fails.
    await evaluate("localStorage.removeItem('tyde.loader.host-target.v1');localStorage.removeItem('tyde.loader.follow-host.v1');return true;");
    await navigate("/tyde/");
    published = true;

    await navigate("/tyde/");
    assert.equal(await waitFor(`Client ${capable}`),true,"new root loader must ignore the stale pin once");
    assert.equal(await evaluate("return window.__tydeLoader.bootVersion()"),capable);
    assert.equal(await evaluate("return localStorage.getItem('tyde.loader.follow-host.v1')"),"1");
    assert.equal(await evaluateAsync("const done=arguments[arguments.length-1];navigator.serviceWorker.getRegistration().then(async registration=>{await registration.update();const worker=registration.installing||registration.waiting;if(!worker||worker.state==='activated'){done(true);return;}worker.addEventListener('statechange',()=>{if(worker.state==='activated'){done(true);}});});"),true);
    const cacheNames = await evaluateAsync("const done=arguments[arguments.length-1];caches.keys().then(done);");
    assert.ok(cacheNames.includes("tyde-loader-v13"),"updated worker precaches the new root shell");
    assert.ok(!cacheNames.includes("tyde-loader-v12"),"updated worker retires only the old shell cache");
    await t.test("vendor decoder is shell state, never verified bundle state", async () => {
      const decoderUrl = "/tyde/vendor/jsqr.js";
      await page.evaluate(async url => {
        const bundle = await caches.open('tyde-bundle-v1');
        await bundle.put(url, new Response('stale decoder bytes'));
      }, decoderUrl);
      const bytes = await page.evaluate(async url => Array.from(new Uint8Array(
        await (await fetch(url)).arrayBuffer())), decoderUrl);
      assert.ok(Buffer.from(bytes).equals(decoderBytes),
        "the worker must fetch current decoder bytes instead of a stale bundle-cache entry");
      const identity = await page.evaluate(async path => {
        const script = document.querySelector(`script[src="./${path}"]`);
        const bytes = await (await fetch(script.src)).arrayBuffer();
        const hex = Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', bytes)),
          byte => byte.toString(16).padStart(2, '0')).join('');
        const sri = 'sha384-' + btoa(String.fromCharCode(...new Uint8Array(
          await crypto.subtle.digest('SHA-384', bytes))));
        return {path: `decoder/jsqr-${hex}.js`, sri, declared: script.integrity,
          decoderAvailable: typeof window.jsQR === 'function'};
      }, decoderPath);
      assert.equal(decoderPath, identity.path, "decoder publication identity must name its exact bytes");
      assert.equal(identity.declared, identity.sri, "the executed decoder must have matching SRI");
      assert.ok(identity.decoderAvailable, "the browser must load the pinned decoder");
      await page.evaluate(async url => (await caches.open('tyde-bundle-v1')).delete(url), decoderUrl);
    });
    const retained = await evaluateAsync(`const done=arguments[arguments.length-1];const r=indexedDB.open('tyde-mobile',1);r.onsuccess=()=>{const db=r.result;const tx=db.transaction(['paired_hosts','psk']);const a=tx.objectStore('paired_hosts').get('all');const b=tx.objectStore('psk').get('retained-key');tx.oncomplete=()=>{done([a.result,b.result]);db.close();};};`);
    assert.ok(retained[0]===JSON.stringify([{localHostId:"retained-host"},{localHostId:"second-host"},{localHostId:"third-host"}]) && retained[1]==="fixture-only-key","pairing and key stores remain byte-identical");
    const prepare = (version, protocol=66) => evaluateAsync(`const done=arguments[arguments.length-1];window.__tydeLoader.prepareHostSwitch(${JSON.stringify(version)},${protocol},'retained-host').then(done);`);
    assert.deepEqual(await prepare("1.0.2"),{status:"unavailable",reason:"unpublished"});
    assert.deepEqual(await prepare(old,67),{status:"unavailable",reason:"protocol"});
    blocked=true; assert.deepEqual(await prepare(old),{status:"unavailable",reason:"policy"}); blocked=false;
    await evaluateAsync("const done=arguments[arguments.length-1];caches.delete('tyde-bundle-v1').then(done);");
    tamper=true; assert.deepEqual(await prepare(old),{status:"unavailable",reason:"integrity"}); tamper=false;
    offline=true;
    assert.deepEqual(await prepare(old),{status:"unavailable",reason:"manifest"});
    offline=false;
    let prepared = await prepare(old);
    assert.equal(prepared.status,"ready");
    assert.deepEqual(await evaluate(`window.__tydeLoader.cancelHostSwitch();return window.__tydeLoader.commitHostSwitch(${prepared.ticket},'retained-host');`),{status:"unavailable",reason:"cancelled"});
    prepared = await prepare(old);
    assert.deepEqual(await evaluate(`const original=Storage.prototype.setItem;try {Storage.prototype.setItem=()=>{throw new Error('denied');};return window.__tydeLoader.commitHostSwitch(${prepared.ticket},'retained-host');} finally {Storage.prototype.setItem=original;}`),{status:"unavailable",reason:"storage"});
    for (let attempt=0;attempt<2;attempt++) {
      prepared = await prepare(old);
      assert.deepEqual(await evaluate(`return window.__tydeLoader.commitHostSwitch(${prepared.ticket},'retained-host',()=>{});`),{status:"reloading"});
    }
    prepared = await prepare(old);
    assert.deepEqual(await evaluate(`return window.__tydeLoader.commitHostSwitch(${prepared.ticket},'retained-host',()=>{});`),{status:"unavailable",reason:"reload_loop"});
    assert.deepEqual(await evaluate(`return window.__tydeLoader.confirmHostRelease('${capable}',66,'retained-host');`),{status:"matching"});
    const before = manifestRequests;
    const ready = await prepare(old);
    assert.equal(ready.status,"ready");assert.ok(manifestRequests>before,"preparation must fetch fresh policy");
    assert.equal(await evaluate("return document.getElementById('app-root').textContent"),`Client ${capable}`,"preflight cannot unmount UI");
    await evaluate(`window.__tydeLoader.commitHostSwitch(${ready.ticket},'retained-host');return true;`);
    assert.equal(await waitFor(`Client ${old}`),true);
    await navigate("/tyde/");
    assert.equal(await waitFor(`Client ${old}`),true,"intentional old-host choice must not re-migrate");
    await t.test("exact boot retains target owner", async () => {
      assert.equal(await evaluate("return JSON.parse(localStorage.getItem('tyde.loader.host-target.v1')).host"),'retained-host');
    });
    offline=true;
    await navigate("/tyde/");
    assert.equal(await waitFor("Could not load the release manifest"),true);
    assert.equal(await evaluate("return document.getElementById('app-root').textContent"),"","cold offline fails closed");
    offline=false;blocked=true;
    await navigate("/tyde/");
    assert.equal(await waitFor("blocked for safety"),true,"known target never falls back to latest");
    assert.equal(await evaluate("return JSON.parse(localStorage.getItem('tyde.loader.host-target.v1')).version"),old);
    await t.test("deferred selection persists B before A is revoked", async () => {
      blocked=false;await navigate("/tyde/");assert.equal(await waitFor(`Client ${old}`),true);
      assert.deepEqual(await evaluate(`window.__tydeLoader.selectHost('second-host');return window.__tydeLoader.recordHostRelease('${capable}',66,'second-host');`),{status:"matching"});
      assert.equal(await evaluate("return document.getElementById('app-root').textContent"),`Client ${old}`,"recording authority cannot unmount the deferred UI");
      blocked=true;await navigate("/tyde/");
      assert.equal(await waitFor(`Client ${capable}`),true,"revoked A must not gate the selected B target");
      assert.equal(await evaluate(`const target=JSON.parse(localStorage.getItem('tyde.loader.host-target.v1'));return target?.host==='second-host' && target?.version==='${capable}';`),true,"B must remain the exact owned target, not a latest fallback");
    });
    await t.test("forgotten owner cannot gate remaining hosts picker bootstrap", async () => {
      await evaluate(`localStorage.setItem('tyde.loader.host-target.v1',JSON.stringify({host:'retained-host',version:'${old}',protocolVersion:66}));localStorage.setItem('tyde.loader.version','${old}');localStorage.setItem('tyde.selected-host.v1','retained-host');return true;`);
      assert.equal(await evaluateAsync(`const done=arguments[arguments.length-1];const r=indexedDB.open('tyde-mobile',1);r.onsuccess=()=>{const db=r.result;const tx=db.transaction('paired_hosts','readwrite');tx.objectStore('paired_hosts').put(JSON.stringify([{localHostId:'second-host'},{localHostId:'third-host'}]),'all');tx.oncomplete=()=>{db.close();done(true);};};`),true);
      blocked=true;await navigate("/tyde/");
      assert.equal(await waitFor(`Client ${capable}`),true,"forgotten A must not block boot with two remaining pairings");
      assert.equal(await evaluate("return localStorage.getItem('tyde.loader.host-target.v1')"),null);
      assert.equal(await evaluate("return localStorage.getItem('tyde.selected-host.v1')"),null,"loader must leave host choice to picker");
    });
    await t.test("direct origin upgrades its served client without inventing host authority", async () => {
      blocked = false;
      servedClient = capable;
      await navigate("/tyde/");
      assert.equal(await waitFor(`Client ${capable}`),true);
      assert.deepEqual(await evaluate(`window.__tydeLoader.selectHost('second-host');return window.__tydeLoader.confirmHostRelease('${capable}',66,'second-host');`),{status:"matching"});
      const handoff = await evaluate("return localStorage.getItem('tyde.loader.host-target.v1');");
      const pin = await evaluate("return localStorage.getItem('tyde.loader.version');");
      const pairing = await evaluateAsync(`const done=arguments[arguments.length-1];const r=indexedDB.open('tyde-mobile',1);r.onsuccess=()=>{const db=r.result;const tx=db.transaction(['paired_hosts','psk']);const hosts=tx.objectStore('paired_hosts').get('all');const key=tx.objectStore('psk').get('retained-key');tx.oncomplete=()=>{done([hosts.result,key.result]);db.close();};};`);
      servedClient = upgraded;
      for (let launch = 0; launch < 2; launch++) {
        await navigate("/tyde/");
        assert.equal(await waitFor(`Client ${upgraded}`),true,"an upgraded origin must boot its declared client even when the old pin has another protocol");
        assert.equal(await evaluate("return window.__tydeLoader.bootVersion();"),upgraded);
        assert.equal(await evaluate("return localStorage.getItem('tyde.loader.host-target.v1');"),handoff,"loading code is not a host announcement");
        assert.equal(await evaluate("return localStorage.getItem('tyde.loader.version');"),pin,"a disconnected boot must retain the last confirmed host pin");
        assert.equal(await evaluate("return localStorage.getItem('tyde.selected-host.v1');"),'second-host');
      }
      const afterUpgrade = await evaluateAsync(`const done=arguments[arguments.length-1];const r=indexedDB.open('tyde-mobile',1);r.onsuccess=()=>{const db=r.result;const tx=db.transaction(['paired_hosts','psk']);const hosts=tx.objectStore('paired_hosts').get('all');const key=tx.objectStore('psk').get('retained-key');tx.oncomplete=()=>{done([hosts.result,key.result]);db.close();};};`);
      assert.ok(afterUpgrade[0] === pairing[0] && afterUpgrade[1] === pairing[1],"cold upgrades must preserve pairing and key stores byte-for-byte");
      assert.deepEqual(await evaluate(`return window.__tydeLoader.confirmHostRelease('${upgraded}',66,'second-host');`),{status:"unavailable",reason:"protocol"},"stale protocol authority cannot confirm the new client");
      assert.deepEqual(await evaluate(`return window.__tydeLoader.confirmHostRelease('${upgraded}',67,'second-host');`),{status:"matching"});
      await navigate("/tyde/");
      assert.equal(await waitFor(`Client ${upgraded}`),true);
      assert.equal(await evaluate("return JSON.parse(localStorage.getItem('tyde.loader.host-target.v1')).protocolVersion;"),67);
    });
    await t.test("direct client startup fails closed without discarding host pins", async () => {
      servedClient = upgraded;
      const handoff = await evaluate("return localStorage.getItem('tyde.loader.host-target.v1');");
      const pin = await evaluate("return localStorage.getItem('tyde.loader.version');");
      const cases = [
        { policy: { servedClientRelease: capable }, text: "release manifest is malformed" },
        { policy: { servedClientRelease: null }, text: "release manifest is malformed" },
        { policy: { minSupported: capable }, text: "release manifest is malformed" },
        { entry: { followsSelectedHost: undefined }, text: "release manifest is malformed" },
        { entry: { protocolVersion: undefined }, text: "mobile client has not been published" },
        { policy: { blocked: [upgraded] }, text: "blocked for safety" },
        { tamper: true, text: "failed its integrity check" },
        { offline: true, text: "Could not load the release manifest" },
      ];
      for (const scenario of cases) {
        directPolicy = scenario.policy || {};
        directEntry = scenario.entry || {};
        tamper = scenario.tamper || false;
        offline = scenario.offline || false;
        if (tamper) await evaluateAsync("const done=arguments[arguments.length-1];caches.delete('tyde-bundle-v1').then(done);");
        await navigate("/tyde/");
        assert.equal(await waitFor(scenario.text),true);
        assert.equal(await evaluate("return document.getElementById('app-root').textContent;"),"","a rejected declared client must never execute another version");
        assert.equal(await evaluate("return localStorage.getItem('tyde.loader.host-target.v1');"),handoff);
        assert.equal(await evaluate("return localStorage.getItem('tyde.loader.version');"),pin);
      }
      directPolicy = {}; directEntry = {}; tamper = false; offline = false;
      await navigate("/tyde/");
      assert.equal(await waitFor(`Client ${upgraded}`),true,"retry must read repaired policy rather than stay wedged");
      await writeFile(manifestPath, JSON.stringify(manifest()));
      const checker = new URL("../../../tools/check_mobile_web_manifest.py", import.meta.url).pathname;
      const args = [checker,upgraded,"--manifest",manifestPath,"--protocol-source",protocolSource];
      await promisify(execFile)("python3",[...args,"--direct-hosted"]);
      await assert.rejects(promisify(execFile)("python3",args),error => error.stderr.includes("servedClientRelease"),"a direct startup declaration cannot be deployed to the shared origin");
      await writeFile(manifestPath,JSON.stringify({...manifest(),servedClientRelease:capable}));
      await assert.rejects(promisify(execFile)("python3",[...args,"--direct-hosted"]),error => error.stderr.includes("servedClientRelease"),"the release guard must reject an incorrect startup declaration");
    });
    await t.test("thin scanner preserves blank video frames and renders decode failures without a camera", async () => {
      scannerOnly = true;
      const scanner = await browser.newPage({viewport:{width:390,height:844},isMobile:true,hasTouch:true});
      try {
        scanner.setDefaultTimeout(0);
        await scanner.goto(origin + '/tyde/');
        await scanner.locator('#view-pair').waitFor({state:'visible'});
        const geometry = await scanner.evaluate(() => ({
          viewportHeight: innerHeight, documentHeight: document.documentElement.scrollHeight,
          loaderHeight: document.getElementById('loader-shell').getBoundingClientRect().height,
          emptyAppHeight: document.getElementById('app-root').getBoundingClientRect().height,
        }));
        assert.ok(process.env.TYDE_NATIVE_CURRENT_CLIENT,'the canonical check must own scanner evidence');
        const evidence = join(process.env.TYDE_NATIVE_CURRENT_CLIENT,'..','scanner-evidence');
        await mkdir(evidence,{recursive:true});
        const failure = await scanner.evaluate(() => new Promise(resolve => {
          // A real DOM MediaStream exercises the consumer, not camera capture or permission.
          const canvas = document.createElement('canvas');
          canvas.width = canvas.height = 80;
          document.body.append(canvas);
          canvas.getContext('2d').fillRect(0,0,80,80);
          const stream = canvas.captureStream(0);
          const track = stream.getVideoTracks()[0];
          if (typeof track.requestFrame !== 'function') throw Error('Explicit canvas frame capture is unavailable');
          document.getElementById('scan-video').addEventListener('play', () => {
            canvas.getContext('2d').fillRect(0,0,80,80);
            track.requestFrame();
          }, {once:true});
          const originalRequest = navigator.mediaDevices.getUserMedia;
          const originalDecoder = window.jsQR;
          delete window.BarcodeDetector;
          navigator.mediaDevices.getUserMedia = async () => stream;
          let blank;
          window.jsQR = (...args) => {
            if (blank === undefined) {
              const result = originalDecoder(...args);
              blank = result === null;
              return result;
            }
            queueMicrotask(() => {
              const status = document.getElementById('scan-status');
              const video = document.getElementById('scan-video');
              const result = {blank,message:status.textContent,role:status.getAttribute('role'),
                stopped:stream.getTracks().every(track=>track.readyState==='ended'),
                detached:video.srcObject===null,hidden:video.hidden};
              stream.getTracks().forEach(track=>track.stop());
              navigator.mediaDevices.getUserMedia = originalRequest;
              window.jsQR = originalDecoder;
              canvas.remove();resolve(result);
            });
            throw Error('private decoder detail');
          };
          document.getElementById('scan-button').click();
        }));
        assert.deepEqual(failure,{blank:true,message:'QR scanning failed. Try again, or paste the pairing link.',
          role:'alert',stopped:true,detached:true,hidden:true},
          'a running scanner must surface decode failure and release its real MediaStream');
        await scanner.evaluate(() => { delete window.jsQR; });
        await scanner.locator('#scan-button').click();
        // Await the real button handler's settled state, not a camera permission prompt.
        await scanner.waitForFunction(() => document.getElementById('scan-status').textContent.length > 0);
        assert.equal(await scanner.locator('#scan-status').textContent(),
          'The QR decoder is unavailable. Try again, or paste the pairing link.');
        assert.equal(await scanner.locator('#scan-status').getAttribute('role'),'alert');
        assert.equal(await scanner.locator('#scan-video').isVisible(),false);
        assert.equal(await scanner.locator('#paste-input').isVisible(),true);
        assert.equal(await scanner.locator('#connect-button').isEnabled(),true);
        await writeFile(join(evidence,'isolated-dom-geometry.json'),JSON.stringify(geometry,null,2));
        await scanner.screenshot({path:join(evidence,'decoder-unavailable.png')});
        console.info('Scanner real-DOM evidence: '+evidence);
      } finally { await scanner.close(); }
    });
  } finally {
    try {
      if (browser) await browser.close();
    } finally {
      server.closeAllConnections();await new Promise(resolve=>server.close(resolve));
    }
  }
});

// The native loader runs before Wasm can own connection state. Missing local
// material, a stale host registration and an actual rejection must stay distinct.
test("native discovery surfaces credential loss and host absence without inventing revocation", async () => {
  assert.ok(process.env.CHROME, "dev.sh must provision Chrome");
  const root = new URL("../", import.meta.url);
  let mode = "missing", requests = 0;
  const server = createServer(async (req, res) => {
    const path = new URL(req.url, "http://owned.invalid").pathname;
    if (path === "/setup") { res.setHeader("content-type", "text/html"); res.end("<!doctype html><title>Owned discovery fixture</title>"); return; }
    if (path.endsWith("/bootstrap/resolve")) {
      requests++;
      req.resume();
      res.setHeader("content-type", "application/json");
      const code = mode === "stale" ? "bootstrap_stale" : mode === "revoked" ? "pairing_revoked" : "service_unavailable";
      res.writeHead(mode === "unavailable" ? 503 : 403).end(JSON.stringify({error:{code,message:"owned HTTP boundary",retryable:mode !== "revoked"}}));
      return;
    }
    try {
      if (!path.startsWith("/tyde/") || path.includes("..")) throw Error("asset path");
      const relative = path === "/tyde/" ? "index.html" : path.slice(6);
      const bytes = await readFile(new URL(relative, root));
      const type = relative.endsWith(".html") ? "text/html" : relative.endsWith(".css") ? "text/css" : relative.endsWith(".js") ? "text/javascript" : "image/svg+xml";
      res.setHeader("content-type",type); res.end(bytes);
    } catch { res.writeHead(404).end(); }
  });
  server.listen(0, "127.0.0.1"); await once(server,"listening");
  let browser;
  try {
    browser = await chromium.launch({executablePath:process.env.CHROME,args:["--no-sandbox"]});
    for (mode of ["missing","locked","stale","unavailable","revoked"]) {
      const context = await browser.newContext();
      const page = await context.newPage();
      const before = requests;
      await context.addInitScript(mode => {
        window.__tydeNativeHost = {version:6,bootstrapVersion:1,shellRelease:"0.9.6-beta.25",post(endpoint) {
          if (endpoint === "presentation") return Promise.resolve(null);
          if (endpoint === "secret_read") {
            if (mode === "locked") return Promise.reject(new Error("Owned platform storage denial"));
            return Promise.resolve(mode === "missing" ? null : "owned-fixture-only-secret");
          }
          return Promise.reject(new Error("Unexpected owned native request"));
        }};
      }, mode);
      const origin = `http://127.0.0.1:${server.address().port}`;
      await page.goto(origin+"/setup");
      await page.evaluate(async () => {
        await new Promise((resolve,reject) => {
          const open = indexedDB.open("tyde-mobile",1);
          open.onupgradeneeded=()=>{open.result.createObjectStore("paired_hosts");open.result.createObjectStore("psk");};
          open.onerror=()=>reject(Error("Owned storage setup failed"));
          open.onsuccess=()=>{
            const db=open.result,tx=db.transaction("paired_hosts","readwrite");
            tx.objectStore("paired_hosts").put(JSON.stringify([{localHostId:"owned-host",hostLabel:"Owned host",credentialFingerprint:"public-owned-fingerprint",autoConnect:true,lastConnectedAtMs:null,managed:{pairingId:"owned-pair",deviceId:"owned-device",deviceSecretKeyId:"tyde-web-owned-secret"}}]),"all");
            tx.oncomplete=()=>{db.close();localStorage.setItem("tyde.selected-host.v1","owned-host");resolve();};
          };
        });
      });
      await page.goto(origin+"/tyde/");
      await page.locator("#view-error").waitFor({state:"visible"});
      const text=await page.locator("#view-error").innerText();
      const expected = {missing:"pairing credential is missing on this device",locked:"could not access credential storage",stale:"host’s mobile access registration has expired",unavailable:"Host discovery is temporarily unavailable",revoked:"This pairing has been revoked"};
      assert.ok(text.includes(expected[mode]),`the real loader must surface the specific ${mode} failure`);
      if(mode!=="revoked")assert.ok(!text.toLowerCase().includes("revoked"),"neither silence nor local storage failure establishes revocation");
      assert.equal(requests-before,mode==="missing"||mode==="locked"?0:1,"local credential failure must stop before authenticated HTTP");
      const outcome=await page.evaluate(()=>window.__tydeLoader.nativeAdmitHost("owned-host"));
      assert.deepEqual(outcome,{status:"unavailable",reason:{missing:"pairing_secret_missing",locked:"credential_storage_unavailable",stale:"host_unavailable",unavailable:"bootstrap_transport",revoked:"pairing_revoked"}[mode]});
      assert.equal(await page.evaluate(()=>localStorage.getItem("tyde.selected-host.v1")),"owned-host");
      await context.close();
    }
  } finally {
    if(browser)await browser.close();
    server.closeAllConnections(); await new Promise(resolve=>server.close(resolve));
  }
});
