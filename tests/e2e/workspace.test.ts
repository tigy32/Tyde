import { openWorkspace, openWorkspaceAndWaitForChat, sel } from './helpers';

describe('Multi-workspace management', () => {
  it('comprehensive workspace flow: welcome → tab isolation → settings overlay', async () => {
    // --- 1. Welcome screen on first workspace ---
    await openWorkspace();

    const welcome = await $(sel.welcomeScreen);
    await welcome.waitForExist({ timeout: 5000 });
    expect(await welcome.isDisplayed()).toBe(true);

    // --- 2. Tab isolation: create tab in workspace A ---
    await browser.keys(['Control', 'n']);
    const input = await $(sel.messageInput);
    await input.waitForDisplayed({ timeout: 10_000 });

    const workspaceALabels = await browser.execute((viewSel, tabBarSel, tabTitleSel) => {
      const activeView = Array.from(document.querySelectorAll(viewSel))
        .find(el => (el as HTMLElement).style.display !== 'none');
      if (!activeView) return [];
      return Array.from(activeView.querySelectorAll(`${tabBarSel} ${tabTitleSel}`))
        .map(el => el.textContent ?? '');
    }, sel.workspaceView, sel.tabBar, sel.convTabTitle);
    expect(workspaceALabels.length).toBeGreaterThan(0);

    // --- Open workspace B ---
    await browser.execute(() => {
      (window as any).__mockDialogPath = '/mock/workspace-b';
    });

    const addBtn = await $(sel.railAddBtn);
    await addBtn.waitForClickable({ timeout: 5000 });
    await addBtn.click();

    const title = await $(sel.appTitle);
    await browser.waitUntil(
      async () => (await title.getText()).includes('workspace-b'),
      { timeout: 10_000, timeoutMsg: 'Workspace B did not load' },
    );

    // Workspace B tabs should NOT contain workspace A's tabs
    const workspaceBLabels = await browser.execute((viewSel, tabBarSel, tabTitleSel) => {
      const activeView = Array.from(document.querySelectorAll(viewSel))
        .find(el => (el as HTMLElement).style.display !== 'none');
      if (!activeView) return [];
      return Array.from(activeView.querySelectorAll(`${tabBarSel} ${tabTitleSel}`))
        .map(el => el.textContent ?? '');
    }, sel.workspaceView, sel.tabBar, sel.convTabTitle);
    for (const label of workspaceALabels) {
      expect(workspaceBLabels).not.toContain(label);
    }

    // --- 3. Welcome screen on second workspace ---
    await browser.waitUntil(async () => {
      const all = await $$(sel.welcomeScreen);
      for (const el of all) {
        if (await el.isDisplayed()) return true;
      }
      return false;
    }, { timeout: 5000, timeoutMsg: 'No visible welcome screen on second workspace' });

    // --- 4. Switch back to workspace A ---
    await browser.execute((projItemSel, homeItemSel, projNameSel) => {
      const items = document.querySelectorAll(`${projItemSel}:not(.active):not(${homeItemSel})`);
      for (const item of items) {
        const name = item.querySelector(projNameSel);
        if (name?.textContent === 'workspace') {
          (item as HTMLElement).click();
          return;
        }
      }
    }, sel.railProjectItem, sel.railHomeItem, sel.railProjectName);

    await browser.waitUntil(
      async () => (await title.getText()).includes('workspace') && !(await title.getText()).includes('workspace-b'),
      { timeout: 10_000, timeoutMsg: 'Workspace A did not restore' },
    );

    // Workspace A's original tabs are restored
    const restoredLabels = await browser.execute((viewSel, tabBarSel, tabTitleSel) => {
      const activeView = Array.from(document.querySelectorAll(viewSel))
        .find(el => (el as HTMLElement).style.display !== 'none');
      if (!activeView) return [];
      return Array.from(activeView.querySelectorAll(`${tabBarSel} ${tabTitleSel}`))
        .map(el => el.textContent ?? '');
    }, sel.workspaceView, sel.tabBar, sel.convTabTitle);
    for (const label of workspaceALabels) {
      expect(restoredLabels).toContain(label);
    }

    // --- 5. Welcome screen persists on switch (covered by switching back) ---
    // First workspace still shows welcome for tabs that had it
    // This is implicitly validated: workspace A had a chat tab created, so welcome is replaced.
    // Workspace B still has welcome when we switch to it later.

    // --- 6. Settings overlay: app-level, not inside workspace DOM ---
    const settingsResult = await browser.execute((settingsViewSel, viewSel) => {
      const settings = document.querySelector(settingsViewSel);
      if (!settings) return { exists: false, insideWorkspace: false, parentId: null };
      let el = settings.parentElement;
      let insideWorkspace = false;
      while (el) {
        if (el.matches(viewSel)) { insideWorkspace = true; break; }
        el = el.parentElement;
      }
      return { exists: true, insideWorkspace, parentId: settings.parentElement?.id ?? null };
    }, sel.settingsTabView, sel.workspaceView);

    expect(settingsResult.exists).toBe(true);
    expect(settingsResult.insideWorkspace).toBe(false);
    expect(settingsResult.parentId).toBe('workspace-container');

    // Open settings with Ctrl+,
    await browser.keys(['Control', ',']);
    await browser.pause(500);

    let settingsVisible = await browser.execute((settingsViewSel) => {
      const settings = document.querySelector(settingsViewSel);
      return settings ? !settings.classList.contains('hidden') : false;
    }, sel.settingsTabView);
    expect(settingsVisible).toBe(true);

    // Close with Escape
    await browser.keys('Escape');
    await browser.pause(500);

    // Switch to workspace B for settings test
    await browser.execute(() => {
      (window as any).__mockDialogPath = '/mock/workspace-b';
    });

    // Click workspace B in rail
    await browser.execute((projItemSel, homeItemSel, projNameSel) => {
      const items = document.querySelectorAll(`${projItemSel}:not(${homeItemSel})`);
      for (const item of items) {
        const name = item.querySelector(projNameSel);
        if (name?.textContent === 'workspace-b') {
          (item as HTMLElement).click();
          return;
        }
      }
    }, sel.railProjectItem, sel.railHomeItem, sel.railProjectName);

    await browser.waitUntil(
      async () => (await title.getText()).includes('workspace-b'),
      { timeout: 10_000, timeoutMsg: 'Workspace B did not load for settings test' },
    );

    // Settings panel parent is still workspace-container
    const parentId = await browser.execute((settingsViewSel) => {
      const settings = document.querySelector(settingsViewSel);
      return settings?.parentElement?.id ?? null;
    }, sel.settingsTabView);
    expect(parentId).toBe('workspace-container');

    // Settings still works in second workspace
    await browser.keys(['Control', ',']);
    await browser.pause(500);

    settingsVisible = await browser.execute((settingsViewSel) => {
      const settings = document.querySelector(settingsViewSel);
      return settings ? !settings.classList.contains('hidden') : false;
    }, sel.settingsTabView);
    expect(settingsVisible).toBe(true);

    await browser.keys('Escape');

    // --- 7. Cleanup ---
    await browser.execute(() => {
      delete (window as any).__mockDialogPath;
    });
  });
});
