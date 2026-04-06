import { openWorkspace, sel } from "./helpers";

describe("Workbench (git worktree)", () => {
  it("comprehensive workbench flow: create via context menu, switch, rename, remove", async () => {
    // --- 1. Open a workspace to have a parent project ---
    await openWorkspace();

    const title = await $(sel.appTitle);
    await browser.waitUntil(
      async () => (await title.getText()).includes("workspace"),
      { timeout: 10_000, timeoutMsg: "Workspace did not load" },
    );

    // Confirm the project appears in the rail
    const railItems = await $$(sel.railProjectItem);
    const parentItem = railItems.find(async (item) => {
      const name = await item.$(sel.railProjectName);
      return (await name.getText()) === "workspace";
    });
    expect(parentItem).toBeDefined();

    // --- 2. Right-click parent project → "New Workbench" ---
    // Find the parent project item in the rail by name
    const parentEl = await browser.execute(
      (projItemSel, homeItemSel, projNameSel) => {
        const items = document.querySelectorAll(
          `${projItemSel}:not(${homeItemSel})`,
        );
        for (const item of items) {
          const name = item.querySelector(projNameSel);
          if (name?.textContent === "workspace") {
            const rect = item.getBoundingClientRect();
            return { x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 };
          }
        }
        return null;
      },
      sel.railProjectItem,
      sel.railHomeItem,
      sel.railProjectName,
    );

    expect(parentEl).not.toBeNull();

    // Simulate right-click to open context menu
    await browser.execute(
      (projItemSel, homeItemSel, projNameSel) => {
        const items = document.querySelectorAll(
          `${projItemSel}:not(${homeItemSel})`,
        );
        for (const item of items) {
          const name = item.querySelector(projNameSel);
          if (name?.textContent === "workspace") {
            const rect = item.getBoundingClientRect();
            item.dispatchEvent(
              new MouseEvent("contextmenu", {
                bubbles: true,
                clientX: rect.x + rect.width / 2,
                clientY: rect.y + rect.height / 2,
              }),
            );
            return;
          }
        }
      },
      sel.railProjectItem,
      sel.railHomeItem,
      sel.railProjectName,
    );

    // Wait for context menu to appear
    const contextMenu = await $(".rail-context-menu");
    await contextMenu.waitForDisplayed({ timeout: 5_000 });

    // Verify "New Workbench" option is visible
    const newWorkbenchItem = await $(sel.railContextNewWorkbench);
    expect(await newWorkbenchItem.isDisplayed()).toBe(true);

    // Click "New Workbench"
    await newWorkbenchItem.click();

    // --- 3. Fill in the branch name prompt ---
    const promptInput = await $(sel.textPromptInput);
    await promptInput.waitForDisplayed({ timeout: 5_000 });

    // Type branch name
    await promptInput.setValue("feature-test");

    // Click confirm
    const confirmBtn = await $(sel.textPromptConfirm);
    await confirmBtn.click();

    // --- 4. Verify the workbench appears in the rail ---
    await browser.waitUntil(
      async () => (await (await $$(sel.railWorkbenchItem)).length) > 0,
      { timeout: 5_000, timeoutMsg: "Workbench item did not appear in rail" },
    );

    // The workbench should be named after the branch
    const workbenchName = await browser.execute(
      (wbSel, nameSel) => {
        const item = document.querySelector(wbSel);
        const name = item?.querySelector(nameSel);
        return name?.textContent ?? null;
      },
      sel.railWorkbenchItem,
      sel.railProjectName,
    );
    expect(workbenchName).toBe("feature-test");

    // The app title should reflect the new workbench
    await browser.waitUntil(
      async () => (await title.getText()).includes("feature-test"),
      {
        timeout: 5_000,
        timeoutMsg: "App title did not update to workbench name",
      },
    );

    // --- 5. The workbench has its own welcome screen (no tabs from parent) ---
    await browser.waitUntil(
      async () => {
        const all = await $$(sel.welcomeScreen);
        for (const el of all) {
          if (await el.isDisplayed()) return true;
        }
        return false;
      },
      { timeout: 5000, timeoutMsg: "Workbench should show welcome screen" },
    );

    // --- 6. Switch back to parent workspace ---
    await browser.execute(
      (projItemSel, homeItemSel, projNameSel) => {
        const items = document.querySelectorAll(
          `${projItemSel}:not(${homeItemSel})`,
        );
        for (const item of items) {
          const name = item.querySelector(projNameSel);
          if (name?.textContent === "workspace") {
            (item as HTMLElement).click();
            return;
          }
        }
      },
      sel.railProjectItem,
      sel.railHomeItem,
      sel.railProjectName,
    );

    await browser.waitUntil(
      async () => {
        const text = await title.getText();
        return text.includes("workspace") && !text.includes("feature-test");
      },
      { timeout: 5_000, timeoutMsg: "Did not switch back to parent workspace" },
    );

    // --- 7. Switch to home view and verify workbench appears in project grid ---
    const homeItem = await $(sel.railHomeItem);
    await homeItem.click();

    await browser.waitUntil(
      async () => {
        const hv = await $(sel.homeView);
        return hv.isDisplayed();
      },
      { timeout: 5_000 },
    );

    // Both parent and workbench should appear as project cards
    const projectNames = await browser.execute(
      (cardSel, nameSel) => {
        return Array.from(
          document.querySelectorAll(`${cardSel} ${nameSel}`),
        ).map((el) => el.textContent ?? "");
      },
      sel.projectCard,
      sel.projectName,
    );
    expect(projectNames).toContain("workspace");
    expect(projectNames).toContain("feature-test");

    // --- 8. Right-click workbench → rename ---
    // Switch to the workbench first
    await browser.execute((wbSel) => {
      const item = document.querySelector(wbSel);
      if (item) (item as HTMLElement).click();
    }, sel.railWorkbenchItem);

    await browser.waitUntil(
      async () => (await title.getText()).includes("feature-test"),
      { timeout: 5_000 },
    );

    // Right-click workbench to open context menu
    await browser.execute((wbSel) => {
      const item = document.querySelector(wbSel);
      if (item) {
        const rect = item.getBoundingClientRect();
        item.dispatchEvent(
          new MouseEvent("contextmenu", {
            bubbles: true,
            clientX: rect.x + rect.width / 2,
            clientY: rect.y + rect.height / 2,
          }),
        );
      }
    }, sel.railWorkbenchItem);

    // Wait for context menu
    const wbContextMenu = await $(".rail-context-menu");
    await wbContextMenu.waitForDisplayed({ timeout: 5_000 });

    // Verify "Remove Workbench" option exists
    const removeItem = await $(sel.railContextRemoveWorkbench);
    expect(await removeItem.isDisplayed()).toBe(true);

    // Click "Rename" (first item in workbench context menu)
    const renameItem = await browser.execute(() => {
      const items = document.querySelectorAll(".rail-context-menu-item");
      for (const item of items) {
        if (item.textContent === "Rename") {
          (item as HTMLElement).click();
          return true;
        }
      }
      return false;
    });
    expect(renameItem).toBe(true);

    // Fill in the new name
    const renameInput = await $(sel.textPromptInput);
    await renameInput.waitForDisplayed({ timeout: 5_000 });
    await renameInput.setValue("my-workbench");
    const renameConfirm = await $(sel.textPromptConfirm);
    await renameConfirm.click();

    // Verify the name changed in the rail
    await browser.waitUntil(
      async () => {
        const name = await browser.execute(
          (wbSel, nameSel) => {
            const item = document.querySelector(wbSel);
            return item?.querySelector(nameSel)?.textContent ?? null;
          },
          sel.railWorkbenchItem,
          sel.railProjectName,
        );
        return name === "my-workbench";
      },
      { timeout: 5_000, timeoutMsg: "Workbench name did not update in rail" },
    );

    // --- 9. Remove workbench via context menu ---
    await browser.execute((wbSel) => {
      const item = document.querySelector(wbSel);
      if (item) {
        const rect = item.getBoundingClientRect();
        item.dispatchEvent(
          new MouseEvent("contextmenu", {
            bubbles: true,
            clientX: rect.x + rect.width / 2,
            clientY: rect.y + rect.height / 2,
          }),
        );
      }
    }, sel.railWorkbenchItem);

    const removeMenu = await $(".rail-context-menu");
    await removeMenu.waitForDisplayed({ timeout: 5_000 });

    const removeBtn = await $(sel.railContextRemoveWorkbench);
    await removeBtn.click();

    // Verify workbench is gone from the rail
    await browser.waitUntil(
      async () => (await (await $$(sel.railWorkbenchItem)).length) === 0,
      { timeout: 5_000, timeoutMsg: "Workbench was not removed from rail" },
    );

    // Should have switched away (to parent or home)
    await browser.waitUntil(
      async () => !(await title.getText()).includes("my-workbench"),
      { timeout: 5_000, timeoutMsg: "Title still shows removed workbench" },
    );
  });
});
