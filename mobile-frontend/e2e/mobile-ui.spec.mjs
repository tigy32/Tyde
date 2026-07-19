import { expect, test } from "@playwright/test";

const FIXTURE_STATES = [
  "onboarding",
  "home",
  "chat",
  "chat-light",
  "disconnected",
  "error",
];

async function openFixture(page, name) {
  await page.goto(`/?tyde-fixture=${name}`);
  await page.waitForFunction(() => window.__TYDE_FIXTURE_READY__ === true);
  await expect(page.locator(".mobile-app")).toHaveAttribute(
    "data-mobile-fixture",
    name,
  );
}

test("@visual captures deterministic phone states", async ({ page }, testInfo) => {
  for (const name of FIXTURE_STATES) {
    await openFixture(page, name);
    await page.screenshot({
      path: testInfo.outputPath(`${name}.png`),
      fullPage: true,
    });
  }
});

test("photo selection previews and sends the image", async ({ page }, testInfo) => {
  await openFixture(page, "chat");

  const picker = page.locator("[data-mobile-test='chat-photo-input']");
  await picker.setInputFiles({
    name: "phone-photo.png",
    mimeType: "image/png",
    buffer: Buffer.from(
      "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
      "base64",
    ),
  });

  await expect(page.locator("[data-mobile-test='chat-photo-tray'] img")).toHaveCount(1);
  await page.screenshot({
    path: testInfo.outputPath("photo-attachment.png"),
    fullPage: true,
  });
  await expect(page.locator("[data-mobile-test='chat-send']")).toBeEnabled();
  await page.locator("[data-mobile-test='chat-send']").click();

  await expect
    .poll(() =>
      page.evaluate(() => window.__TYDE_FIXTURE_SENT_LINES__?.length ?? 0),
    )
    .toBe(1);
  const sent = await page.evaluate(() =>
    JSON.parse(window.__TYDE_FIXTURE_SENT_LINES__[0]),
  );
  expect(sent.kind).toBe("send_message");
  expect(sent.payload.images).toHaveLength(1);
  expect(sent.payload.images[0].media_type).toBe("image/png");
  await expect(page.locator("[data-mobile-test='chat-photo-tray']")).toHaveCount(0);
});

test("photo selection can be removed before sending", async ({ page }) => {
  await openFixture(page, "chat");
  await page.locator("[data-mobile-test='chat-photo-input']").setInputFiles({
    name: "discard.png",
    mimeType: "image/png",
    buffer: Buffer.from(
      "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
      "base64",
    ),
  });

  await page.getByRole("button", { name: "Remove discard.png" }).click();
  await expect(page.locator("[data-mobile-test='chat-photo-tray']")).toHaveCount(0);
  await expect(page.locator("[data-mobile-test='chat-send']")).toBeDisabled();
});
