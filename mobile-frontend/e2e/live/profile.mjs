import { chromium, devices } from "@playwright/test";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { readFile } from "node:fs/promises";
import { authenticateMobileFixture } from "./e2e-oauth.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
export const REPO_ROOT = resolve(HERE, "../../..");
const STANDARD_PROFILE_DIR = resolve(
  REPO_ROOT,
  ".tyde-playwright/mobile-live-profile",
);
const E2E_PROFILE_DIR = resolve(
  REPO_ROOT,
  ".tyde-playwright/mobile-live-e2e-profile",
);
export const PROFILE_DIR =
  process.env.TYDE_LIVE_E2E_OAUTH === "1"
    ? E2E_PROFILE_DIR
    : STANDARD_PROFILE_DIR;
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

export async function authenticateLiveContext(context) {
  if (process.env.TYDE_LIVE_E2E_OAUTH !== "1") {
    return;
  }
  const packageJson = JSON.parse(
    await readFile(resolve(REPO_ROOT, "package.json"), "utf8"),
  );
  const protocolSource = await readFile(
    resolve(REPO_ROOT, "protocol/src/types.rs"),
    "utf8",
  );
  const protocolMatch = protocolSource.match(
    /pub const PROTOCOL_VERSION: u32 = (\d+);/,
  );
  if (!protocolMatch) {
    throw new Error("Could not read the Tyde protocol version");
  }
  await authenticateMobileFixture({
    request: context.request,
    fixtureId: process.env.TYDE_E2E_FIXTURE_ID ?? "active-pass-p1",
    releaseVersion: packageJson.version,
    protocolVersion: Number(protocolMatch[1]),
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
