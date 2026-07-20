import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./mobile-frontend/e2e/live",
  outputDir: "./test-results/mobile-playwright-live",
  fullyParallel: false,
  workers: 1,
  timeout: 90_000,
  expect: { timeout: 45_000 },
  reporter: [
    ["list"],
    [
      "html",
      {
        outputFolder: "test-results/mobile-playwright-live-report",
        open: "never",
      },
    ],
  ],
  use: {
    screenshot: "only-on-failure",
    trace: "retain-on-failure",
    video: "retain-on-failure",
  },
});
