import { Miniflare } from "miniflare";
import { fileURLToPath } from "node:url";

const server = new Miniflare({
  modules: true,
  modulesRoot: fileURLToPath(new URL("./", import.meta.url)),
  scriptPath: fileURLToPath(new URL("./worker.mjs", import.meta.url)),
  compatibilityDate: "2026-07-30",
  durableObjects: { BROKER: { className: "DotsBroker", useSQLite: true } },
  port: 0,
});
console.log((await server.ready).origin);
process.stdin.resume();
async function stop() { await server.dispose(); process.exit(0); }
process.stdin.on("end", stop);
process.on("SIGTERM", stop);
