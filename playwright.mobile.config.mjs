import { defineConfig, devices } from "@playwright/test";

export default defineConfig({
  testDir: "./mobile-frontend/e2e",
  outputDir: "./test-results/mobile-playwright",
  fullyParallel: false,
  timeout: 30_000,
  expect: { timeout: 5_000 },
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
  webServer: {
    command: "npm run mobile:ui:serve",
    url: "http://127.0.0.1:4173/?tyde-fixture=onboarding",
    reuseExistingServer: !process.env.CI,
    timeout: 120_000,
    stdout: "pipe",
    stderr: "pipe",
  },
});
