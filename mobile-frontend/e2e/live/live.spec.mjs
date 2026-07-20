import { test as base, expect } from "@playwright/test";
import {
  LIVE_URL,
  launchLiveContext,
  liveSessionStatus,
} from "./profile.mjs";

const test = base.extend({
  liveContext: [
    async ({}, use) => {
      const context = await launchLiveContext({
        headless: process.env.TYDE_LIVE_HEADED !== "1",
      });
      await use(context);
      await context.close();
    },
    { scope: "worker" },
  ],
  page: async ({ liveContext }, use) => {
    const pages = liveContext.pages();
    const page = pages[0] ?? (await liveContext.newPage());
    await use(page);
  },
});

test("saved user is authenticated and connected", async ({ page }, testInfo) => {
  const browserErrors = [];
  page.on("pageerror", (error) => browserErrors.push(error.message));

  await page.goto(LIVE_URL, { waitUntil: "domcontentloaded" });
  await expect(page.locator("#boot-error")).toHaveCount(0);

  const session = await liveSessionStatus(page);
  expect(
    session.status,
    "Run `npm run mobile:live:login` and complete Tyggs login first",
  ).toBe(200);

  await expect(
    page.locator("[data-mobile-test='connection-banner-dot-connected']"),
    "Start a desktop Tyde pairing offer and pair this persistent browser profile",
  ).toHaveCount(1);
  await expect(page.locator("[data-mobile-test='connection-banner-rtt']")).toContainText(
    /Checking|\d+ ms/,
  );

  const connectedScreenshot = testInfo.outputPath("connected.png");
  await page.screenshot({ path: connectedScreenshot, fullPage: true });

  await page.reload({ waitUntil: "domcontentloaded" });
  await expect(
    page.locator("[data-mobile-test='connection-banner-dot-connected']"),
    "The saved pairing should reconnect after a full page reload",
  ).toHaveCount(1);
  expect(browserErrors, "The live app emitted browser errors").toEqual([]);
});
