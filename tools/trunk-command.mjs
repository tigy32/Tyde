#!/usr/bin/env node

import { spawn, spawnSync } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { existsSync, lstatSync, mkdirSync, readlinkSync, symlinkSync } from "node:fs";
import { readFile, realpath } from "node:fs/promises";
import { createServer } from "node:http";
import { once } from "node:events";
import { dirname, extname, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(scriptDir, "..");
const trunkConfig = resolve(repoRoot, "frontend", "Trunk.toml");

const [command, ...args] = process.argv.slice(2);
if (!command || !["build", "serve", "dev-instance"].includes(command)) {
  console.error("Usage: node tools/trunk-command.mjs <build|serve|dev-instance> [trunk args...]");
  process.exit(2);
}

const env = { ...process.env };
if (env.NO_COLOR === "1") {
  env.NO_COLOR = "true";
}

if (!env.CARGO_TARGET_DIR && repoRoot.includes("%")) {
  const targetDir = resolve(repoRoot, "target");
  const repositoryHash = createHash("sha256").update(repoRoot).digest("hex");
  const aliasPath = `/tmp/tyde-wasm-target-${repositoryHash}`;
  mkdirSync(targetDir, { recursive: true });
  if (existsSync(aliasPath) || lstatSync(aliasPath, { throwIfNoEntry: false })) {
    if (!lstatSync(aliasPath).isSymbolicLink() || readlinkSync(aliasPath) !== targetDir) {
      console.error(`Wasm target alias is occupied by another path: ${aliasPath}`);
      process.exit(1);
    }
  } else {
    symlinkSync(targetDir, aliasPath);
  }
  env.CARGO_TARGET_DIR = aliasPath;
}

if (command === "dev-instance") {
  try {
    await serveDevInstance();
  } catch (error) {
    console.error(`Owned frontend startup failed: ${error.message}`);
    process.exit(1);
  }
} else {
  const result = spawnSync("trunk", [command, ...args, "--config", trunkConfig], {
    cwd: repoRoot,
    env,
    shell: process.platform === "win32",
    stdio: "inherit",
  });

  if (result.error) {
    console.error(result.error.message);
    process.exit(1);
  }

  process.exit(result.status ?? 1);
}

async function serveDevInstance() {
  if (args.length !== 2 || args[0] !== "--dist") {
    throw new Error("dev-instance requires exactly --dist <isolated directory>");
  }
  const dist = resolve(args[1]);
  // An existing output must never certify a new build.
  mkdirSync(dist);
  const contract = JSON.parse(await readFile(resolve(repoRoot, "devtools-protocol/dev-instance-startup.json")));
  const notify = event => process.stdout.write(JSON.stringify({ protocol: contract.frontend, event }) + "\n");
  const build = spawn("trunk", ["build", "--dist", dist, "--config", trunkConfig], {
    cwd: repoRoot,
    env,
    shell: process.platform === "win32",
    stdio: ["ignore", process.stderr, process.stderr],
  });
  const [exitCode, signal] = await once(build, "close");
  notify({ kind: "build_finished", exit_code: exitCode, signal });
  if (exitCode !== 0 || signal !== null) {
    throw new Error(`Frontend initial build failed: exit_code=${exitCode} signal=${signal}`);
  }
  const root = await realpath(dist);
  const index = await readFile(resolve(root, "index.html"));
  if (index.length === 0) throw new Error("Frontend build produced an empty index");
  const indexSha256 = createHash("sha256").update(index).digest("hex");
  const identity = randomBytes(32).toString("hex");
  const contentTypes = {
    ".html": "text/html", ".js": "text/javascript", ".css": "text/css",
    ".wasm": "application/wasm", ".json": "application/json", ".svg": "image/svg+xml",
    ".png": "image/png", ".ico": "image/x-icon", ".woff2": "font/woff2",
  };
  const server = createServer(async (request, response) => {
    if (request.method !== "GET" && request.method !== "HEAD") {
      response.writeHead(405).end();
      return;
    }
    // Reject DNS-rebinding hosts, not just non-loopback bind addresses.
    if (request.headers.host !== `127.0.0.1:${server.address().port}`) {
      response.writeHead(403).end();
      return;
    }
    let file;
    try {
      const path = decodeURIComponent(new URL(request.url, "http://localhost").pathname);
      file = resolve(root, `.${path === "/" ? "/index.html" : path}`);
      if (!file.startsWith(root + sep)) throw new Error("Path outside isolated output");
    } catch {
      response.writeHead(400).end();
      return;
    }
    try {
      const canonical = await realpath(file);
      if (!canonical.startsWith(root + sep)) {
        response.writeHead(403).end();
        return;
      }
      const body = await readFile(canonical);
      response.writeHead(200, {
        "Content-Type": contentTypes[extname(canonical)] ?? "application/octet-stream",
        "Content-Length": body.length,
        "Cache-Control": "no-store",
        "X-Content-Type-Options": "nosniff",
        "X-Tyde-Frontend-Identity": identity,
      });
      response.end(request.method === "HEAD" ? undefined : body);
    } catch (error) {
      const missing = error.code === "ENOENT" || error.code === "ENOTDIR" || error.code === "EISDIR";
      if (!missing) console.error(`Owned frontend asset read failed: ${error.message}`);
      response.writeHead(missing ? 404 : 500).end();
    }
  });
  // The producer selects and retains the actual endpoint. No reserve/release gap.
  const listening = once(server, "listening");
  server.listen(0, "127.0.0.1");
  await listening;
  server.on("error", error => {
    console.error(`Owned frontend serving failed: ${error.message}`);
    process.exit(1);
  });
  notify({ kind: "serving", port: server.address().port, identity, index_sha256: indexSha256 });
}
