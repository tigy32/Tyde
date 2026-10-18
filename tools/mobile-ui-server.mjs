import { once } from "node:events";
import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { extname, resolve, sep } from "node:path";

const contentTypes = {
  ".html": "text/html",
  ".js": "text/javascript",
  ".css": "text/css",
  ".wasm": "application/wasm",
  ".json": "application/json",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".jpg": "image/jpeg",
  ".webp": "image/webp",
  ".ico": "image/x-icon",
};

export default async function startMobileUiServer() {
  const dist = resolve("mobile-frontend/dist");
  await readFile(resolve(dist, "index.html"));
  const server = createServer(async (request, response) => {
    if (request.method !== "GET" && request.method !== "HEAD") {
      response.writeHead(405).end();
      return;
    }
    let file;
    try {
      const path = decodeURIComponent(new URL(request.url, "http://localhost").pathname);
      file = resolve(dist, `.${path === "/" ? "/index.html" : path}`);
      if (!file.startsWith(dist + sep)) throw new Error("Path outside fixture output");
    } catch {
      response.writeHead(400).end();
      return;
    }
    try {
      const body = await readFile(file);
      response.writeHead(200, {
        "Content-Type": contentTypes[extname(file)] ?? "application/octet-stream",
        "Cache-Control": "no-store",
      });
      response.end(request.method === "HEAD" ? undefined : body);
    } catch (error) {
      const missing = error.code === "ENOENT" || error.code === "EISDIR";
      if (!missing) console.error("Mobile UI fixture read failed", error);
      response.writeHead(missing ? 404 : 500).end();
    }
  });
  const listening = once(server, "listening");
  server.listen(0, "127.0.0.1");
  await listening;
  const url = `http://127.0.0.1:${server.address().port}`;
  // Playwright's builtin baseURL fixture reads this after global setup.
  process.env.PLAYWRIGHT_TEST_BASE_URL = url;
  console.log(`Mobile UI fixture listening at ${url}`);
  return async () => {
    await new Promise((resolve, reject) => {
      server.close(error => error ? reject(error) : resolve());
    });
    if (process.env.PLAYWRIGHT_TEST_BASE_URL === url) delete process.env.PLAYWRIGHT_TEST_BASE_URL;
    console.log(`Mobile UI fixture closed at ${url}`);
  };
}
