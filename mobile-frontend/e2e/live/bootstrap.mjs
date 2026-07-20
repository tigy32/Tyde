import { mkdir } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import {
  LIVE_URL,
  PROFILE_DIR,
  REPO_ROOT,
  launchLiveContext,
  liveSessionStatus,
} from "./profile.mjs";

await mkdir(PROFILE_DIR, { recursive: true });
const context = await launchLiveContext({ headless: false });
const pages = context.pages();
const page = pages[0] ?? (await context.newPage());
await page.goto(LIVE_URL, { waitUntil: "domcontentloaded" });

console.log(`
Tyde live browser opened.

1. Complete Tyggs sign-in in the browser if requested.
2. In desktop Tyde, open Settings → Mobile and start pairing.
3. In the browser, use Paste pairing URI (recommended for this harness) or Scan QR.
4. Wait until the connection indicator says Connected.
5. Return here and press Enter. The profile remains local at:
   ${PROFILE_DIR}
`);

process.stdin.resume();
await new Promise((resolveInput) => process.stdin.once("data", resolveInput));

const session = await liveSessionStatus(page).catch((error) => ({
  status: 0,
  body: { error: String(error) },
}));
const connected = await page
  .locator("[data-mobile-test='connection-banner-dot-connected']")
  .count();
const screenshotPath = resolve(
  REPO_ROOT,
  "test-results/mobile-playwright-live/bootstrap.png",
);
await mkdir(dirname(screenshotPath), { recursive: true });
await page.screenshot({ path: screenshotPath, fullPage: true });

console.log(
  JSON.stringify(
    {
      authenticated: session.status === 200,
      authStatus: session.status,
      connected: connected > 0,
      screenshot: screenshotPath,
    },
    null,
    2,
  ),
);
await context.close();
