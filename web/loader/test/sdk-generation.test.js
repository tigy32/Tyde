import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { once } from 'node:events';
import { createHash } from 'node:crypto';
import { readFile, writeFile, mkdtemp, mkdir, cp, rm, readdir } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, extname } from 'node:path';
import { chromium } from 'playwright';
import { assembleShell } from '../../deploy/assemble-shell.mjs';

const sha = bytes => createHash('sha256').update(bytes).digest('hex');
async function files(root, prefix = '') {
  const result = new Map();
  for (const entry of await readdir(join(root, prefix), {withFileTypes:true})) {
    const name = prefix + entry.name;
    if (entry.isDirectory()) {
      for (const [path, bytes] of await files(root, name + '/')) result.set(path, bytes);
    } else result.set(name, await readFile(join(root, name)));
  }
  return result;
}

test('SDK publication retains one module closure per URL through forward and rollback selection', async t => {
  assert.ok(process.env.CHROME, 'dev.sh provisions the browser');
  const temporary = await mkdtemp(join(tmpdir(), 'tyde-sdk-generation-'));
  t.after(() => rm(temporary, {recursive:true, force:true}));
  const source = join(temporary, 'source');
  await cp(new URL('../', import.meta.url), source, {recursive:true, filter:path => !/\/(test|node_modules)(\/|$)/.test(path)});
  const publish = async name => {
    const out = join(temporary, name);
    await mkdir(out);
    await assembleShell(source, out);
    return files(out);
  };
  await writeFile(join(source, 'native-bootstrap.js'), (await readFile(join(source, 'native-bootstrap.js'), 'utf8')) + '\nimport "./generation-probe.js";\n');
  await writeFile(join(source, 'generation-probe.js'), 'globalThis.sdkGenerationProbe = "A";\n');
  const a = await publish('a');
  await writeFile(join(source, 'generation-probe.js'), 'globalThis.sdkGenerationProbe = "B";\n');
  const b = await publish('b');
  let selected = a;
  const retained = new Map(a);
  const requests = [];
  const server = createServer((req, res) => {
    const path = new URL(req.url, 'http://fixture.invalid').pathname.slice('/tyde/'.length);
    if (path === 'probe.html') {
      res.setHeader('Content-Type', 'text/html'); res.setHeader('Cache-Control', 'no-store');
      res.end('<!doctype html><title>SDK boundary</title>'); return;
    }
    const bytes = path === '' ? selected.get('index.html') : path === 'sw.js' ? selected.get(path) : retained.get(path);
    res.setHeader('Cache-Control', path === '' || path === 'sw.js' ? 'no-store' : 'public,max-age=14400');
    res.setHeader('Content-Type', extname(path) === '.js' ? 'text/javascript' : extname(path) === '.css' ? 'text/css' : 'text/html');
    res.statusCode = bytes ? 200 : 404;
    requests.push({path, status:res.statusCode, sha256:bytes ? sha(bytes) : null});
    res.end(bytes);
  });
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  const browser = await chromium.launch({executablePath:process.env.CHROME, headless:true, args:['--no-sandbox'], timeout:0});
  t.after(async () => { await browser.close(); await new Promise(resolve => server.close(resolve)); });
  const origin = `http://127.0.0.1:${server.address().port}`;
  const observed = new Map();
  const open = async () => {
    const context = await browser.newContext();
    const page = await context.newPage(); page.setDefaultTimeout(0); page.setDefaultNavigationTimeout(0);
    await page.goto(origin + '/tyde/probe.html');
    await page.evaluate(async () => {
      const changed = new Promise(resolve => navigator.serviceWorker.addEventListener('controllerchange', resolve, {once:true}));
      await navigator.serviceWorker.register('./sw.js', {scope:'./'});
      await navigator.serviceWorker.ready;
      if (!navigator.serviceWorker.controller) await changed;
      localStorage.setItem('sdk-retained-pairing-sentinel', 'retained');
    });
    return {context, page};
  };
  const selection = page => page.evaluate(async () => {
    const doc = new DOMParser().parseFromString(await (await fetch('./', {cache:'no-store'})).text(), 'text/html');
    return new URL('./native-bootstrap.js', new URL(doc.querySelector('script[type="module"]').getAttribute('src'), location.href)).href;
  });
  const execute = async (page, url, expected) => {
    const imported = await page.evaluate(async url => {
      const module = await import(url);
      return {start:typeof module.startNativeBootstrap, probe:globalThis.sdkGenerationProbe, pairing:localStorage.getItem('sdk-retained-pairing-sentinel')};
    }, url);
    assert.deepEqual(imported, {start:'function', probe:expected, pairing:'retained'});
    for (const record of requests.filter(item => item.status === 200 && item.path.endsWith('.js') && item.path !== 'sw.js')) {
      if (observed.has(record.path)) assert.equal(record.sha256, observed.get(record.path), 'One SDK URL must never name different bytes across publications: ' + record.path);
      observed.set(record.path, record.sha256);
    }
  };
  const warm = await open(); const oldUrl = await selection(warm.page);
  await execute(warm.page, oldUrl, 'A');
  selected = b; for (const entry of b) retained.set(...entry);
  const cold = await open(); const newUrl = await selection(cold.page);
  await execute(cold.page, newUrl, 'B');
  assert.notEqual(oldUrl, newUrl, 'A changed closure needs a new generation identity');
  await execute(warm.page, await selection(warm.page), 'B');
  const retainedPage = await warm.context.newPage(); retainedPage.setDefaultTimeout(0);
  await retainedPage.goto(origin + '/tyde/probe.html');
  await execute(retainedPage, oldUrl, 'A');
  selected = a;
  const rollback = await warm.context.newPage(); rollback.setDefaultTimeout(0);
  await rollback.goto(origin + '/tyde/probe.html');
  assert.equal(await selection(rollback), oldUrl);
  await execute(rollback, oldUrl, 'A');
  const fault = await open();
  selected = b;
  const missing = [...b.keys()].find(path => path.endsWith('/generation-probe.js'));
  retained.delete(missing);
  const missingUrl = await selection(fault.page);
  const failure = await fault.page.evaluate(async url => {
    try { await import(url); return {failed:false}; }
    catch { return {failed:true, evaluated:globalThis.sdkGenerationProbe !== undefined}; }
  }, missingUrl);
  assert.deepEqual(failure, {failed:true, evaluated:false}, 'An incomplete generation must reject linking, never execute another closure');
  assert.ok(requests.some(record => record.path === missing && record.status === 404));
  console.info('SDK boundary receipt: cold A/B, warm forward, retained A after B, rollback A, missing member fails before evaluation; HTTP max-age=14400, real worker/modules, no storage clearing');
});
