#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, lstatSync, mkdirSync, readlinkSync, symlinkSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(scriptDir, "..");
const trunkConfig = resolve(repoRoot, "frontend", "Trunk.toml");

const [command, ...args] = process.argv.slice(2);
if (!command || !["build", "serve"].includes(command)) {
  console.error("Usage: node tools/trunk-command.mjs <build|serve> [trunk args...]");
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
