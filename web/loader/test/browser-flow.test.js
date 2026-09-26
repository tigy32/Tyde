import { test } from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { readFile, writeFile, mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { spawn, execFile } from "node:child_process";
import { once } from "node:events";

// Real browser + HTTP + CacheStorage + IndexedDB + worker lifecycle. The tiny
// immutable modules are executable loader fixtures, not fake agent backends.
test("installed storage migrates once, then follows exact targets without losing pairing", { timeout: 90000 }, async t => {
  assert.ok(process.env.CHROME && process.env.CHROMEDRIVER, "dev.sh must provision browser tools");
  const root = new URL("../", import.meta.url);
  const old = "1.0.0", capable = "1.0.1";
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
  for (const version of [old,capable]) {
    const dist = join(directory,version);await mkdir(dist);
    await writeFile(join(dist,"app.js"),code(version));
    await writeFile(join(dist,"app_bg.wasm"),wasm);
    await writeFile(join(dist,"index.html"),'<!doctype html>'+(version===capable?marker:"")+'<script type="module">import init from "/tyde/v'+version+'/app.js";</script>');
    await promisify(execFile)(process.execPath,[new URL("../../deploy/generate-manifest.mjs",import.meta.url).pathname,
      "--dist",dist,"--version",version,"--manifest",manifestPath,"--protocol-source",protocolSource]);
  }
  const generated = JSON.parse(await readFile(manifestPath,"utf8"));
  assert.equal(generated.versions[old].followsSelectedHost,undefined,"historical artifact gains no capability during backfill");
  assert.equal(generated.versions[capable].followsSelectedHost,1);
  let published = false, legacy = true, offline = false, tamper = false, blocked = false;
  let manifestRequests = 0;
  const manifest = () => {
    const value = structuredClone(generated);
    value.blocked = blocked ? [old] : [];
    if (!published) delete value.versions[capable].followsSelectedHost;
    return value;
  };
  const setup = `window.setupReady = new Promise((resolve,reject) => {
    const open=indexedDB.open('tyde-mobile',1);
    open.onupgradeneeded=()=>{open.result.createObjectStore('paired_hosts');open.result.createObjectStore('psk');};
    open.onerror=()=>reject(new Error('storage setup failed'));
    open.onsuccess=()=>{const db=open.result;const tx=db.transaction(['paired_hosts','psk'],'readwrite');
      tx.objectStore('paired_hosts').put(JSON.stringify([{localHostId:'retained-host'},{localHostId:'second-host'},{localHostId:'third-host'}]),'all');
      tx.objectStore('psk').put('fixture-only-key','retained-key');
      tx.oncomplete=()=>{db.close();localStorage.setItem('tyde.loader.version','${old}');resolve(true);};};
  });`;
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
  const assets = new Set(["index.html", "loader.js", "loader.css", "sw.js", "mobile-service-config.js", "cbor.js", "pairing.js", "pairing-ui.js", "styles.js", "manifest-policy.js", "integrity.js", "manifest.webmanifest", "vendor/jsqr.js", "icons/icon.svg"]);
  const server = createServer(async (req, res) => {
    try {
      const path = new URL(req.url, "http://fixture.invalid").pathname;
      res.setHeader("Cache-Control", "no-store");
      if (path === "/setup") { res.setHeader("Content-Type","text/html"); res.end('<script src="/setup.js"></script>'); return; }
      if (path === "/setup.js") { res.setHeader("Content-Type","text/javascript"); res.end(setup); return; }
      if (path === "/tyde/manifest.json") {
        manifestRequests++;
        res.setHeader("Content-Type","application/json");
        res.statusCode = offline ? 503 : 200;
        res.end(JSON.stringify(manifest())); return;
      }
      const artifact = /^\/tyde\/v(1\.0\.[01])\/(app\.js|app_bg\.wasm|index\.html)$/.exec(path);
      if (artifact) {
        const [, version, file] = artifact;
        res.setHeader("Content-Type", file.endsWith(".js") ? "text/javascript" : file.endsWith(".wasm") ? "application/wasm" : "text/html");
        res.end(file === "index.html" ? "<!doctype html>" : file === "app.js" ? code(version) : tamper ? Buffer.from("tampered") : wasm);
        return;
      }
      const asset = path === "/tyde/" ? "index.html" : path.slice("/tyde/".length);
      if (!path.startsWith("/tyde/") || !assets.has(asset)) { res.statusCode=404;res.end();return; }
      let body = await readFile(new URL(asset, root));
      if (legacy && asset === "loader.js") body = Buffer.from(legacyLoader);
      if (legacy && asset === "sw.js") body = Buffer.from(body.toString().replace("tyde-loader-v10", "tyde-loader-v9"));
      res.setHeader("Content-Type", asset.endsWith(".js") ? "text/javascript" : asset.endsWith(".css") ? "text/css" : asset.endsWith(".svg") ? "image/svg+xml" : asset.endsWith(".html") ? "text/html" : "application/json");
      res.end(body);
    } catch { res.statusCode=500;res.end("fixture failure"); }
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const origin = `http://127.0.0.1:${server.address().port}`;
  const driver = spawn(process.env.CHROMEDRIVER, ["--port=0"], {stdio:["ignore","pipe","pipe"]});
  let session;
  let request;
  try {
    const port = await new Promise((resolve,reject) => {
      const timer = setTimeout(()=>reject(new Error("driver startup deadline")), 10000);
      let output = "";
      const read = chunk => { output += chunk; const match = /started successfully on port (\d+)/.exec(output); if (match) {clearTimeout(timer);resolve(Number(match[1]));} };
      driver.stdout.on("data",read);driver.stderr.on("data",read);
      driver.once("error",error=>{clearTimeout(timer);reject(error);});
    });
    request = async (path, body, method="POST") => {
      const response = await fetch(`http://127.0.0.1:${port}${path}`, {method, headers:{"Content-Type":"application/json"}, ...(body ? {body:JSON.stringify(body)} : {}), signal:AbortSignal.timeout(20000)});
      const result = await response.json();
      if (result.value?.error) throw new Error(`WebDriver ${result.value.error}: ${result.value.message}`);
      return result.value;
    };
    const started = await request("/session", {capabilities:{alwaysMatch:{browserName:"chrome","goog:chromeOptions":{binary:process.env.CHROME,args:["--headless=new","--no-sandbox","--disable-dev-shm-usage"]}}}});
    session = started.sessionId;
    const command = (path, body) => request(`/session/${session}/${path}`,body);
    const evaluate = script => command("execute/sync",{script,args:[]});
    const evaluateAsync = script => command("execute/async",{script,args:[]});
    const navigate = path => command("url",{url:origin+path});
    const waitFor = text => evaluateAsync(`const done=arguments[arguments.length-1];
      const check=()=>document.body.textContent.includes(${JSON.stringify(text)});
      if(check()){done(true);return;} const observer=new MutationObserver(()=>{if(check()){clearTimeout(timer);observer.disconnect();done(true);}});
      const timer=setTimeout(()=>{observer.disconnect();done(false);},10000);observer.observe(document.body,{subtree:true,childList:true,characterData:true});`);
    await navigate("/setup");
    assert.equal(await evaluateAsync("const done=arguments[arguments.length-1];window.setupReady.then(done);"),true);
    await navigate("/tyde/");
    assert.equal(await waitFor(`Client ${old}`),true);
    await evaluateAsync("const done=arguments[arguments.length-1]; navigator.serviceWorker.ready.then(()=>{if(navigator.serviceWorker.controller)done(true);else navigator.serviceWorker.addEventListener('controllerchange',()=>done(true),{once:true});});");
    legacy = false;
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
    assert.equal(await evaluateAsync("const done=arguments[arguments.length-1];navigator.serviceWorker.getRegistration().then(async registration=>{await registration.update();const worker=registration.installing||registration.waiting;if(!worker||worker.state==='activated'){done(true);return;}const timer=setTimeout(()=>done(false),10000);worker.addEventListener('statechange',()=>{if(worker.state==='activated'){clearTimeout(timer);done(true);}});});"),true);
    const cacheNames = await evaluateAsync("const done=arguments[arguments.length-1];caches.keys().then(done);");
    assert.ok(cacheNames.includes("tyde-loader-v10"),"updated worker precaches the new root shell");
    assert.ok(!cacheNames.includes("tyde-loader-v9"),"updated worker retires only the old shell cache");
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
    await request(`/session/${session}`,null,"DELETE");session=null;
  } finally {
    if (session && request) {
      try { await request('/session/'+session,null,"DELETE"); }
      catch { console.error("loader fixture browser-session cleanup failed"); }
    }
    const exited = driver.exitCode !== null ? Promise.resolve() : once(driver,"exit");
    driver.kill("SIGTERM");await exited;
    server.closeAllConnections();await new Promise(resolve=>server.close(resolve));
  }
});
