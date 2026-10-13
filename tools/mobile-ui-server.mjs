import { spawn, spawnSync } from "node:child_process";
import { setTimeout } from "node:timers/promises";

export default async function startMobileUiServer(config) {
  const url = new URL(config.projects[0].use.baseURL);
  // Playwright's webServer treats timeout: 0 as its default 60-second cutoff.
  // Own this foreground server through teardown instead of timing its build.
  try {
    const existing = await fetch(url);
    await existing.body?.cancel();
    throw new Error(`Mobile UI port ${url.port} is already in use; refusing to reuse a server`);
  } catch (error) {
    if (error.cause?.code !== "ECONNREFUSED") throw error;
  }
  const child = spawn("npm", ["run", "mobile:ui:serve"], {
    cwd: process.cwd(),
    stdio: "inherit",
    detached: process.platform !== "win32",
  });
  let stopping = false;
  const exited = new Promise((_, reject) => {
    child.once("error", reject);
    child.once("exit", (code, signal) => {
      if (!stopping) reject(new Error(`Mobile UI server exited before readiness: ${signal ?? code}`));
    });
  });
  const stop = () => {
    stopping = true;
    if (!child.pid) return;
    if (process.platform === "win32") {
      spawnSync("taskkill", ["/pid", String(child.pid), "/t", "/f"], { stdio: "ignore" });
    } else {
      try {
        process.kill(-child.pid, "SIGKILL");
      } catch (error) {
        if (error.code !== "ESRCH") throw error;
      }
    }
  };
  try {
    while (true) {
      const available = await Promise.race([
        fetch(new URL("/?tyde-fixture=onboarding", url))
          .then(async response => {
            await response.body?.cancel();
            return response.ok;
          }, () => false),
        exited,
      ]);
      if (available) {
        exited.catch(error => console.error(error.message));
        return stop;
      }
      await Promise.race([setTimeout(100), exited]);
    }
  } catch (error) {
    stop();
    throw error;
  }
}
