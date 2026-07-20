import { chromium, devices } from "@playwright/test";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

const HERE = dirname(fileURLToPath(import.meta.url));
export const REPO_ROOT = resolve(HERE, "../../..");
export const PROFILE_DIR = resolve(REPO_ROOT, ".tyde-playwright/mobile-live-profile");
export const LIVE_URL = "https://tycode.dev/tyde/";

export async function launchLiveContext({ headless }) {
  const iphone = devices["iPhone 13"];
  return chromium.launchPersistentContext(PROFILE_DIR, {
    headless,
    viewport: iphone.viewport,
    deviceScaleFactor: iphone.deviceScaleFactor,
    hasTouch: iphone.hasTouch,
    isMobile: iphone.isMobile,
    userAgent: iphone.userAgent,
    locale: "en-US",
    colorScheme: "dark",
    serviceWorkers: "allow",
  });
}

export async function liveSessionStatus(page) {
  return page.evaluate(async () => {
    const response = await fetch("/api/tyde/mobile/v1/auth/session", {
      credentials: "include",
      headers: { accept: "application/json" },
    });
    let body = null;
    try {
      body = await response.json();
    } catch {
      body = null;
    }
    return { status: response.status, body };
  });
}
