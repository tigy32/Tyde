import { createHash, randomUUID } from 'node:crypto';
import { mkdir, readdir, readFile, writeFile, rename } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

// The root document selects the entire SDK graph. Relative module imports
// preserve that identity, independently of HTTP/worker cache freshness.
export async function assembleShell(source, out) {
  const assets = new Map();
  async function collect(prefix = '') {
    for (const entry of await readdir(join(source, prefix), {withFileTypes:true})) {
      if (entry.name.startsWith('.') || ['test', 'node_modules', 'package.json', 'manifest.json'].includes(entry.name) || /\.(md|test\.js)$/.test(entry.name)) continue;
      const path = prefix + entry.name;
      if (entry.isDirectory()) await collect(path + '/');
      else if (entry.isFile()) assets.set(path, await readFile(join(source, path)));
      else throw new Error('SDK source must contain only regular files and directories');
    }
  }
  await collect();
  const digest = createHash('sha256').update('tyde-sdk-shell-1\0');
  for (const path of [...assets.keys()].sort()) {
    const bytes = assets.get(path);
    digest.update(JSON.stringify([path, bytes.length])).update(bytes);
  }
  const generation = digest.digest('hex');
  const prefix = `sdk/${generation}/`;
  const graph = [...assets.keys()].filter(path => path.endsWith('.js') || path.endsWith('.css'));
  const write = async (path, bytes, immutable = false) => {
    const target = join(out, path);
    await mkdir(join(target, '..'), {recursive:true});
    if (immutable) {
      try { await writeFile(target, bytes, {flag:'wx'}); }
      catch (error) {
        if (error.code !== 'EEXIST') throw error;
        if (!(await readFile(target)).equals(Buffer.from(bytes))) throw new Error('SDK generation collision: ' + path);
      }
    } else {
      const pending = target + '.' + randomUUID() + '.pending';
      await writeFile(pending, bytes, {flag:'wx'});
      await rename(pending, target);
    }
  };
  for (const path of graph) {
    if (path !== 'sw.js') await write(prefix + path, assets.get(path), true);
  }
  let index = assets.get('index.html').toString();
  index = index.replace(/\b(src|href)=(["'])\.\/([^"']+)\2/g, (attribute, kind, quote, path) => {
    if (graph.includes(path) && path !== 'sw.js') return `${kind}=${quote}./${prefix}${path}${quote}`;
    if (/\.(js|css)$/.test(path)) throw new Error('Unidentified SDK document reference: ' + path);
    return attribute;
  });
  const loaderIntegrity = 'sha384-' + createHash('sha384').update(assets.get('loader.js')).digest('base64');
  let entries = 0;
  index = index.replace(/<script\b(?=[^>]*\btype=["']module["'])[^>]*>/g, tag => {
    if (!tag.includes(`./${prefix}loader.js`)) throw new Error('SDK document must select its identified loader');
    entries++;
    return tag.replace('<script', `<script integrity="${loaderIntegrity}"`);
  });
  if (entries !== 1) throw new Error('SDK document must select exactly one loader');
  const shellAssets = ['./', './index.html', ...graph.filter(path => path !== 'sw.js').map(path => './' + prefix + path), './manifest.webmanifest', './icons/icon.svg'];
  const workerSource = assets.get('sw.js').toString();
  if (!/const SHELL_ASSETS = \[[\s\S]*?\];/.test(workerSource)) throw new Error('SDK worker asset declaration is missing');
  const worker = workerSource
    .replace(/const SHELL_ASSETS = \[[\s\S]*?\];/, `const SHELL_ASSETS = ${JSON.stringify(shellAssets, null, 2)};`);
  for (const [path, bytes] of assets) {
    if (!graph.includes(path) && path !== 'index.html') await write(path, bytes);
  }
  await write('sw.js', worker);
  // Publish this selector only after every generation member exists.
  await write('index.html', index);
  return generation;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  if (process.argv.length !== 4) throw new Error('Usage: assemble-shell.mjs SOURCE OUT');
  console.log(await assembleShell(process.argv[2], process.argv[3]));
}
