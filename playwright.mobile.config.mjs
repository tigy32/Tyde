import { defineConfig, devices } from "@playwright/test";

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
    baseURL: "http://127.0.0.1:4173",
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
