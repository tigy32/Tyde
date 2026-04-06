import { openWorkspace, resetAppState, sel } from "./helpers";

const SSH_URL = "ssh://testuser@remotehost.example.com/home/testuser/project";
const REMOTE_HOST = "testuser@remotehost.example.com";
const APP_READY_TIMEOUT_MS = 30_000;

describe("Remote SSH workspace", () => {
  afterEach(async () => {
    await browser.execute(() => {
      delete (window as any).__mockRemoteFailStep;
    });
  });

  it("full remote connection flow: dialog, completion, failure, and local bypass", async () => {
    // --- Phase 1: SSH success path ---
    await resetAppState();
    const app = await $(sel.app);
    await app.waitForExist({ timeout: 10_000 });

    const remoteBtn = await $(sel.openRemoteBtn);
    await remoteBtn.waitForClickable({ timeout: APP_READY_TIMEOUT_MS });
    await remoteBtn.click();

    // Text prompt appears — type SSH URL and confirm
    const prompt = await $(sel.textPrompt);
    await prompt.waitForExist({ timeout: 5_000 });
    const promptInput = await $(sel.textPromptInput);
    await promptInput.waitForDisplayed({ timeout: 3_000 });
    await promptInput.setValue(SSH_URL);
    const confirmBtn = await $(sel.textPromptConfirm);
    await confirmBtn.click();

    // Connection dialog overlay appears with host and 4 steps
    const dialog = await $(sel.connectionDialog);
    await dialog.waitForExist({ timeout: 5_000 });

    const hostText = await browser.execute((s: string) => {
      return document.querySelector(s)?.textContent ?? "";
    }, sel.connectionHost);
    expect(hostText).toContain(REMOTE_HOST);

    const stepCount = await browser.execute(() => {
      return document.querySelectorAll("[data-step]").length;
    });
    expect(Number(stepCount)).toBe(4);

    // Connection should progress automatically — no manual Ctrl+N needed

    // Completed indicators appear on steps
    await browser.waitUntil(
      async () => {
        return browser.execute(() => {
          const steps = document.querySelectorAll("[data-step]");
          return Array.from(steps).some((s) => {
            const indicator = s.querySelector("div:first-child");
            return indicator?.classList.contains("completed") ?? false;
          });
        });
      },
      { timeout: 5_000, timeoutMsg: "No completed step indicators found" },
    );

    // Dialog auto-dismisses after ready step completes
    await browser.waitUntil(
      async () => {
        return browser.execute((s: string) => {
          return document.querySelector(s) === null;
        }, sel.connectionDialog);
      },
      { timeout: 8_000, timeoutMsg: "Dialog did not auto-dismiss" },
    );

    // Chat textarea available after dialog dismisses
    const chatInput = await $(sel.messageInput);
    await chatInput.waitForDisplayed({ timeout: 5_000 });

    // --- Phase 2: SSH failure path ---
    await resetAppState();
    const app2 = await $(sel.app);
    await app2.waitForExist({ timeout: 10_000 });

    // Wait for home view to fully render before interacting
    const remoteBtn2 = await $(sel.openRemoteBtn);
    await remoteBtn2.waitForClickable({ timeout: APP_READY_TIMEOUT_MS });

    await browser.execute(() => {
      (window as any).__mockRemoteFailStep = "installing_subprocess";
    });

    await remoteBtn2.click();

    const prompt2 = await $(sel.textPrompt);
    await prompt2.waitForExist({ timeout: 5_000 });
    const promptInput2 = await $(sel.textPromptInput);
    await promptInput2.setValue(SSH_URL);
    const confirmBtn2 = await $(sel.textPromptConfirm);
    await confirmBtn2.click();

    const dialog2 = await $(sel.connectionDialog);
    await dialog2.waitForExist({ timeout: 5_000 });

    // Connection attempt should start automatically (will fail at installing_subprocess)

    // Failed indicator appears on the subprocess step
    const failStepSel = sel.connectionStep("installing_subprocess");
    await browser.waitUntil(
      async () => {
        return browser.execute((s: string) => {
          const step = document.querySelector(s);
          const indicator = step?.querySelector("div:first-child");
          return indicator?.classList.contains("failed") ?? false;
        }, failStepSel);
      },
      {
        timeout: 5_000,
        timeoutMsg: "Failed indicator not shown on subprocess step",
      },
    );

    // Close button becomes visible on failure
    const closeBtn = await $(sel.connectionClose);
    await browser.waitUntil(
      async () => {
        const cls = await closeBtn.getAttribute("class");
        return !cls?.includes("hidden");
      },
      { timeout: 3_000, timeoutMsg: "Close button not shown on failure" },
    );

    // Clicking close dismisses the dialog
    await closeBtn.click();
    await browser.waitUntil(
      async () => {
        return browser.execute((s: string) => {
          return document.querySelector(s) === null;
        }, sel.connectionDialog);
      },
      {
        timeout: 3_000,
        timeoutMsg: "Dialog did not dismiss after clicking close",
      },
    );

    // --- Phase 3: Local workspace has no connection dialog ---
    await resetAppState();
    await openWorkspace();

    const hasDialogAfterLocal = await browser.execute((s: string) => {
      return document.querySelector(s) !== null;
    }, sel.connectionDialog);
    expect(hasDialogAfterLocal).toBe(false);
  });
});
