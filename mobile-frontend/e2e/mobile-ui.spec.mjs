import { expect, test } from "@playwright/test";

const FIXTURE_STATES = [
  "onboarding",
  "home",
  "new-chat",
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

test("return inserts a new line instead of sending", async ({ page }) => {
  await openFixture(page, "chat");

  const composer = page.locator("[data-mobile-test='chat-input']");
  await expect(composer).toHaveAttribute("enterkeyhint", "enter");
  await composer.fill("first line");
  await composer.press("Enter");
  await composer.type("second line");

  await expect(composer).toHaveValue("first line\nsecond line");
  await expect
    .poll(() =>
      page.evaluate(() => window.__TYDE_FIXTURE_SENT_LINES__?.length ?? 0),
    )
    .toBe(0);
});

test("new chat sends the selected backend and agent", async ({ page }) => {
  await openFixture(page, "new-chat");

  await page.locator("[data-mobile-test='new-chat-backend']").selectOption("claude");
  await page.locator("[data-mobile-test='new-chat-agent']").selectOption("fixture-reviewer");
  await expect(page.locator("[data-mobile-test='new-chat-options']")).toContainText(
    "Review a change for correctness and regressions.",
  );
  await page.locator("[data-mobile-test='chat-input']").fill("Review this change");
  await page.locator("[data-mobile-test='chat-send']").click();

  await expect
    .poll(() => page.evaluate(() => window.__TYDE_FIXTURE_SENT_LINES__?.length ?? 0))
    .toBe(1);
  const sent = await page.evaluate(() => JSON.parse(window.__TYDE_FIXTURE_SENT_LINES__[0]));
  expect(sent.kind).toBe("spawn_agent");
  expect(sent.payload.custom_agent_id).toBe("fixture-reviewer");
  expect(sent.payload.params.backend_kind).toBe("claude");
});

test("chat composer ignores a stale visual viewport height", async ({ page }) => {
  await page.addInitScript(() => {
    if (window.visualViewport) {
      Object.defineProperty(window.visualViewport, "height", {
        configurable: true,
        get: () => 180,
      });
    }
  });
  await openFixture(page, "chat");

  await expect
    .poll(() =>
      page.evaluate(() => {
        const composer = document.querySelector(
          "[data-mobile-test='chat-input-container']",
        );
        if (!composer) return Number.POSITIVE_INFINITY;
        return Math.abs(composer.getBoundingClientRect().bottom - window.innerHeight);
      }),
    )
    .toBeLessThanOrEqual(1);
});

// Reproduces what iOS WebKit actually does when the software keyboard opens:
// `interactive-widget=resizes-content` is a Chromium-only mitigation, so WebKit
// leaves the layout viewport (window.innerHeight, and therefore 100dvh) at full
// screen height and shrinks ONLY the visual viewport. Before the fix the shell
// stayed 852px tall while 336px of it was behind the keyboard, putting the
// composer out of reach and forcing the browser to pan the whole app off screen
// to reveal the caret.
const KEYBOARD_INSET = 336;

async function installVirtualKeyboard(page) {
  await page.addInitScript(() => {
    window.__KB_INSET__ = 0;
    const viewport = window.visualViewport;
    if (!viewport) return;
    const height = Object.getOwnPropertyDescriptor(
      Object.getPrototypeOf(viewport),
      "height",
    );
    Object.defineProperty(viewport, "height", {
      configurable: true,
      get: () => height.get.call(viewport) - (window.__KB_INSET__ || 0),
    });
  });
}

const setKeyboard = (page, inset) =>
  page.evaluate((value) => {
    window.__KB_INSET__ = value;
    window.visualViewport.dispatchEvent(new Event("resize"));
  }, inset);

const geometry = (page) =>
  page.evaluate(() => {
    const rect = (selector) => {
      const el = document.querySelector(selector);
      return el ? el.getBoundingClientRect() : null;
    };
    const composer = rect("[data-mobile-test='chat-input-container']");
    const shell = rect(".mobile-app");
    return {
      innerHeight: window.innerHeight,
      composerBottom: composer ? composer.bottom : null,
      shellHeight: shell ? shell.height : null,
      messagesHeight: rect(".chat-messages")?.height ?? null,
    };
  });

test("the composer stays above the software keyboard", async ({ page }) => {
  await installVirtualKeyboard(page);
  await openFixture(page, "chat");

  const closed = await geometry(page);
  expect(closed.composerBottom).toBeCloseTo(closed.innerHeight, 0);

  await setKeyboard(page, KEYBOARD_INSET);
  const open = await geometry(page);

  // The layout viewport is untouched — that is the whole point of the case.
  expect(open.innerHeight).toBe(closed.innerHeight);

  const visibleBottom = closed.innerHeight - KEYBOARD_INSET;
  expect(open.composerBottom).toBeLessThanOrEqual(visibleBottom + 1);
  expect(open.shellHeight).toBeLessThanOrEqual(visibleBottom + 1);

  // The transcript absorbs the loss, so the composer is not merely pushed off
  // the top: it is still on screen with history above it.
  expect(open.messagesHeight).toBeLessThan(closed.messagesHeight);
  expect(open.messagesHeight).toBeGreaterThan(0);
});

test("closing the software keyboard restores the shell", async ({ page }) => {
  await installVirtualKeyboard(page);
  await openFixture(page, "chat");

  const closed = await geometry(page);
  await setKeyboard(page, KEYBOARD_INSET);
  await setKeyboard(page, 0);
  const restored = await geometry(page);

  expect(restored.shellHeight).toBeCloseTo(closed.shellHeight, 0);
  expect(restored.composerBottom).toBeCloseTo(closed.composerBottom, 0);
});

test("the document itself never scrolls", async ({ page }) => {
  await installVirtualKeyboard(page);
  await openFixture(page, "chat");
  await setKeyboard(page, KEYBOARD_INSET);

  // Only the transcript may scroll. If the document has scrollable overflow the
  // browser can pan the header off screen, which is the reported symptom.
  const overflow = await page.evaluate(() => {
    const scroller = document.scrollingElement;
    return scroller.scrollHeight - scroller.clientHeight;
  });
  expect(overflow).toBeLessThanOrEqual(1);
});
