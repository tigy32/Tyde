import { defineConfig, devices } from "@playwright/test";
import { createServer } from "node:net";

// Allocate once in the coordinator; Playwright workers inherit the same port.
if (process.env.TYDE_MOBILE_UI_TEST_PORT === undefined) {
  const reservation = createServer();
  await new Promise((resolve, reject) => {
    reservation.once("error", reject);
    reservation.listen(0, "127.0.0.1", resolve);
  });
  const address = reservation.address();
  if (!address || typeof address === "string") throw new Error("Mobile UI port allocation failed");
  process.env.TYDE_MOBILE_UI_TEST_PORT = String(address.port);
  await new Promise((resolve, reject) => reservation.close(error => error ? reject(error) : resolve()));
}
const port = Number(process.env.TYDE_MOBILE_UI_TEST_PORT);
if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error("Invalid mobile UI test port");

export default defineConfig({
  testDir: "./mobile-frontend/e2e",
  outputDir: "./test-results/mobile-playwright",
  fullyParallel: false,
  globalSetup: "./tools/mobile-ui-server.mjs",
  timeout: 600_000,
  expect: { timeout: 0 },
  reporter: [
    ["list"],
    ["html", { outputFolder: "test-results/mobile-playwright-report", open: "never" }],
  ],
  use: {
    baseURL: `http://127.0.0.1:${port}`,
    ...devices["iPhone 13"],
    browserName: "chromium",
    screenshot: "only-on-failure",
    trace: "retain-on-failure",
    video: "retain-on-failure",
  },
  projects: [
    { name: "mobile", testIgnore: "composer-webkit.spec.mjs" },
    { name: "composer-webkit", testMatch: "composer-webkit.spec.mjs", use: { browserName: "webkit", screenshot: "off", trace: "off", video: "off" } },
  ],
});
