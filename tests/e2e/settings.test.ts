import { openWorkspace, openWorkspaceAndWaitForChat, sel } from './helpers';

async function openSettingsOverlay(): Promise<void> {
  await browser.keys(['Control', ',']);
  await browser.pause(500);

  const settingsView = await $(sel.settingsTabView);
  await browser.waitUntil(
    async () => {
      const cls = await settingsView.getAttribute('class');
      return cls !== null && !cls.includes('hidden');
    },
    { timeout: 5_000, timeoutMsg: 'Settings overlay did not become visible' },
  );
}

describe('Settings panel', () => {
  it('overlay opens/closes without creating tabs, backend data loads correctly', async () => {
    await openWorkspaceAndWaitForChat();

    // --- Open settings overlay ---
    await openSettingsOverlay();

    // No settings tab is created in the tab bar
    const hasSettingsTab = await browser.execute((convTabSel: string) => {
      return document.querySelector(convTabSel + '.conv-tab-settings') !== null;
    }, sel.convTab);
    expect(hasSettingsTab).toBe(false);

    // Settings view has nav and tab panels
    const hasRenderedContent = await browser.execute((tabViewSel: string, navSel: string, panelSel: string) => {
      const view = document.querySelector(tabViewSel + ':not(.hidden)');
      if (!view) return { found: false, nav: false, panels: 0 };
      const nav = view.querySelector(navSel) !== null;
      const panels = view.querySelectorAll(panelSel).length;
      return { found: true, nav, panels };
    }, sel.settingsTabView, sel.settingsNav, sel.settingsTabPanel);
    expect(hasRenderedContent.found).toBe(true);
    expect(hasRenderedContent.nav).toBe(true);
    expect(hasRenderedContent.panels).toBeGreaterThanOrEqual(1);

    // --- Close with close button ---
    const closeBtn = await $(sel.settingsClose);
    expect(await closeBtn.isExisting()).toBe(true);
    await closeBtn.click();
    await browser.pause(300);

    let cls = await (await $(sel.settingsTabView)).getAttribute('class');
    expect(cls).toContain('hidden');

    // --- Reopen and close with Escape ---
    await openSettingsOverlay();
    await browser.keys(['Escape']);
    await browser.pause(300);

    cls = await (await $(sel.settingsTabView)).getAttribute('class');
    expect(cls).toContain('hidden');

    // No blank tab remains after closing
    const settingsTabCount = await browser.execute((convTabSel: string) => {
      return document.querySelectorAll(convTabSel + '.conv-tab-settings').length;
    }, sel.convTab);
    expect(settingsTabCount).toBe(0);

    // --- General settings from backend ---
    await openSettingsOverlay();

    await browser.execute((navItemSel: string) => {
      const btn = document.querySelector(navItemSel + '[data-tab="general"]') as HTMLButtonElement | null;
      btn?.click();
    }, sel.settingsNavItem);
    await browser.pause(300);

    await browser.waitUntil(
      async () => {
        return browser.execute((panelSel: string, selectSel: string) => {
          const panel = document.querySelector(panelSel + '[data-panel="general"]');
          if (!panel) return false;
          const first = panel.querySelector(selectSel) as HTMLSelectElement | null;
          return first !== null && first.value === 'unlimited';
        }, sel.settingsTabPanel, sel.settingsSelect);
      },
      { timeout: 5_000, timeoutMsg: 'Settings data did not populate from backend' },
    );

    const selectValues = await browser.execute((panelSel: string, selectSel: string) => {
      const panel = document.querySelector(panelSel + '[data-panel="general"]');
      if (!panel) return [];
      const selects = panel.querySelectorAll(selectSel);
      return Array.from(selects).map(s => (s as HTMLSelectElement).value);
    }, sel.settingsTabPanel, sel.settingsSelect);
    expect(selectValues.length).toBeGreaterThanOrEqual(6);
    expect(selectValues[0]).toBe('unlimited');
    expect(selectValues[1]).toBe('Task');

    // --- Provider cards ---
    await browser.execute((navItemSel: string) => {
      const btn = document.querySelector(navItemSel + '[data-tab="providers"]') as HTMLButtonElement | null;
      btn?.click();
    }, sel.settingsNavItem);
    await browser.pause(300);

    const cardName = await browser.execute((panelSel: string, cardNameSel: string) => {
      const panel = document.querySelector(panelSel + '[data-panel="providers"]');
      if (!panel) return null;
      const name = panel.querySelector(cardNameSel);
      return name ? name.textContent : null;
    }, sel.settingsTabPanel, sel.settingsCardName);
    expect(cardName).toBe('MockProvider');

    // --- Tyde MCP control server toggle ---
    await browser.execute((navItemSel: string) => {
      const btn = document.querySelector(navItemSel + '[data-tab="tyde"]') as HTMLButtonElement | null;
      btn?.click();
    }, sel.settingsNavItem);
    await browser.pause(300);

    const mcpToggle = await browser.execute((panelSel: string) => {
      const panel = document.querySelector(panelSel + '[data-panel="tyde"]');
      if (!panel) return { exists: false, checked: false };
      const input = panel.querySelector('[data-testid="settings-mcp-http-enabled"]') as HTMLInputElement | null;
      if (!input) return { exists: false, checked: false };
      return { exists: true, checked: input.checked };
    }, sel.settingsTabPanel);
    expect(mcpToggle.exists).toBe(true);
    expect(mcpToggle.checked).toBe(true);

    // WebDriver cannot reliably click label-wrapped checkboxes (double-toggle),
    // so toggle via the change event which is how the production handler fires
    await browser.execute((panelSel: string) => {
      const panel = document.querySelector(panelSel + '[data-panel="tyde"]');
      const input = panel?.querySelector('[data-testid="settings-mcp-http-enabled"]') as HTMLInputElement;
      input.checked = !input.checked;
      input.dispatchEvent(new Event('change', { bubbles: true }));
    }, sel.settingsTabPanel);
    await browser.pause(200);

    const mcpToggleAfter = await browser.execute((panelSel: string) => {
      const panel = document.querySelector(panelSel + '[data-panel="tyde"]');
      const input = panel?.querySelector('[data-testid="settings-mcp-http-enabled"]') as HTMLInputElement | null;
      return input?.checked ?? true;
    }, sel.settingsTabPanel);
    expect(mcpToggleAfter).toBe(false);

    const debugMcpToggle = await browser.execute((panelSel: string) => {
      const panel = document.querySelector(panelSel + '[data-panel="tyde"]');
      if (!panel) return { exists: false, checked: true };
      const input = panel.querySelector('[data-testid="settings-driver-mcp-http-enabled"]') as HTMLInputElement | null;
      if (!input) return { exists: false, checked: true };
      return { exists: true, checked: input.checked };
    }, sel.settingsTabPanel);
    expect(debugMcpToggle.exists).toBe(true);
    expect(debugMcpToggle.checked).toBe(false);

    const debugMcpAutoloadInitial = await browser.execute((panelSel: string) => {
      const panel = document.querySelector(panelSel + '[data-panel="tyde"]');
      if (!panel) return { exists: false, checked: true, disabled: false };
      const input = panel.querySelector('[data-testid="settings-driver-mcp-http-autoload"]') as HTMLInputElement | null;
      if (!input) return { exists: false, checked: true, disabled: false };
      return { exists: true, checked: input.checked, disabled: input.disabled };
    }, sel.settingsTabPanel);
    expect(debugMcpAutoloadInitial.exists).toBe(true);
    expect(debugMcpAutoloadInitial.checked).toBe(false);
    expect(debugMcpAutoloadInitial.disabled).toBe(true);

    await browser.execute((panelSel: string) => {
      const panel = document.querySelector(panelSel + '[data-panel="tyde"]');
      const input = panel?.querySelector('[data-testid="settings-driver-mcp-http-enabled"]') as HTMLInputElement;
      input.checked = !input.checked;
      input.dispatchEvent(new Event('change', { bubbles: true }));
    }, sel.settingsTabPanel);
    await browser.pause(200);

    const debugMcpToggleAfter = await browser.execute((panelSel: string) => {
      const panel = document.querySelector(panelSel + '[data-panel="tyde"]');
      const input = panel?.querySelector('[data-testid="settings-driver-mcp-http-enabled"]') as HTMLInputElement | null;
      return input?.checked ?? false;
    }, sel.settingsTabPanel);
    expect(debugMcpToggleAfter).toBe(true);

    const debugMcpAutoloadAfterEnable = await browser.execute((panelSel: string) => {
      const panel = document.querySelector(panelSel + '[data-panel="tyde"]');
      const input = panel?.querySelector('[data-testid="settings-driver-mcp-http-autoload"]') as HTMLInputElement | null;
      if (!input) return { checked: true, disabled: true };
      return { checked: input.checked, disabled: input.disabled };
    }, sel.settingsTabPanel);
    expect(debugMcpAutoloadAfterEnable.checked).toBe(false);
    expect(debugMcpAutoloadAfterEnable.disabled).toBe(false);

    await browser.execute((panelSel: string) => {
      const panel = document.querySelector(panelSel + '[data-panel="tyde"]');
      const input = panel?.querySelector('[data-testid="settings-driver-mcp-http-autoload"]') as HTMLInputElement;
      input.checked = !input.checked;
      input.dispatchEvent(new Event('change', { bubbles: true }));
    }, sel.settingsTabPanel);
    await browser.pause(200);

    const debugMcpAutoloadAfterToggle = await browser.execute((panelSel: string) => {
      const panel = document.querySelector(panelSel + '[data-panel="tyde"]');
      const input = panel?.querySelector('[data-testid="settings-driver-mcp-http-autoload"]') as HTMLInputElement | null;
      return input?.checked ?? false;
    }, sel.settingsTabPanel);
    expect(debugMcpAutoloadAfterToggle).toBe(true);

    // --- Regression: driver toggle state survives saving from another tab ---
    // The bug: env-overridden driver settings were clobbered when any other
    // setting triggered save_app_settings. Verify that switching to "general",
    // changing a setting there, and coming back preserves the driver toggle.
    await browser.execute((navItemSel: string) => {
      const btn = document.querySelector(navItemSel + '[data-tab="general"]') as HTMLButtonElement | null;
      btn?.click();
    }, sel.settingsNavItem);
    await browser.pause(300);

    // Change a general setting (default backend dropdown) to trigger a save
    await browser.execute(() => {
      const select = document.querySelector('[data-testid="default-backend-select"]') as HTMLSelectElement | null;
      if (select && select.options.length > 1) {
        select.value = select.options[1].value;
        select.dispatchEvent(new Event('change', { bubbles: true }));
      }
    });
    await browser.pause(200);

    // Navigate back to tyde tab
    await browser.execute((navItemSel: string) => {
      const btn = document.querySelector(navItemSel + '[data-tab="tyde"]') as HTMLButtonElement | null;
      btn?.click();
    }, sel.settingsNavItem);
    await browser.pause(300);

    // Driver toggle must still be enabled (was toggled on at line ~166 above)
    const driverAfterOtherSave = await browser.execute((panelSel: string) => {
      const panel = document.querySelector(panelSel + '[data-panel="tyde"]');
      const input = panel?.querySelector('[data-testid="settings-driver-mcp-http-enabled"]') as HTMLInputElement | null;
      return input?.checked ?? false;
    }, sel.settingsTabPanel);
    expect(driverAfterOtherSave).toBe(true);

    // Autoload toggle must still be enabled
    const autoloadAfterOtherSave = await browser.execute((panelSel: string) => {
      const panel = document.querySelector(panelSel + '[data-panel="tyde"]');
      const input = panel?.querySelector('[data-testid="settings-driver-mcp-http-autoload"]') as HTMLInputElement | null;
      return input?.checked ?? false;
    }, sel.settingsTabPanel);
    expect(autoloadAfterOtherSave).toBe(true);

    // --- Backends tab ---
    await browser.execute((navItemSel: string) => {
      const btn = document.querySelector(navItemSel + '[data-tab="backends"]') as HTMLButtonElement | null;
      btn?.click();
    }, sel.settingsNavItem);
    await browser.pause(300);

    // Verify backends panel is visible with toggle controls
    const backendsPanel = await browser.execute((panelSel: string) => {
      const panel = document.querySelector(panelSel + '[data-panel="backends"]');
      if (!panel) return { exists: false, toggleCount: 0 };
      const toggles = panel.querySelectorAll('input[type="checkbox"]');
      return { exists: true, toggleCount: toggles.length };
    }, sel.settingsTabPanel);
    expect(backendsPanel.exists).toBe(true);
    expect(backendsPanel.toggleCount).toBe(4);

    // All backends should be enabled by default (assuming deps are available)
    const allEnabled = await browser.execute((panelSel: string) => {
      const panel = document.querySelector(panelSel + '[data-panel="backends"]');
      if (!panel) return false;
      const toggles = panel.querySelectorAll('input[type="checkbox"]:not(:disabled)');
      return Array.from(toggles).every(t => (t as HTMLInputElement).checked);
    }, sel.settingsTabPanel);
    expect(allEnabled).toBe(true);

    // Disable codex backend via change event
    await browser.execute((panelSel: string) => {
      const panel = document.querySelector(panelSel + '[data-panel="backends"]');
      const input = panel?.querySelector('[data-testid="settings-backend-codex-enabled"]') as HTMLInputElement;
      if (input && !input.disabled) {
        input.checked = false;
        input.dispatchEvent(new Event('change', { bubbles: true }));
      }
    }, sel.settingsTabPanel);
    await browser.pause(200);

    // Verify codex toggle is now unchecked
    const codexAfter = await browser.execute((panelSel: string) => {
      const panel = document.querySelector(panelSel + '[data-panel="backends"]');
      const input = panel?.querySelector('[data-testid="settings-backend-codex-enabled"]') as HTMLInputElement | null;
      return input?.checked ?? true;
    }, sel.settingsTabPanel);
    expect(codexAfter).toBe(false);

    // Verify default backend dropdown no longer shows codex
    const backendOptions = await browser.execute(() => {
      const select = document.querySelector('[data-testid="default-backend-select"]') as HTMLSelectElement | null;
      if (!select) return [];
      return Array.from(select.options).map(o => o.value);
    });
    expect(backendOptions).not.toContain('codex');
    expect(backendOptions).toContain('tycode');

    // No install buttons should be visible when all deps are available
    const installBtnCount = await browser.execute((panelSel: string) => {
      const panel = document.querySelector(panelSel + '[data-panel="backends"]');
      if (!panel) return -1;
      return panel.querySelectorAll('.settings-install-btn').length;
    }, sel.settingsTabPanel);
    expect(installBtnCount).toBe(0);

    // Re-enable codex to avoid affecting later tests
    await browser.execute((panelSel: string) => {
      const panel = document.querySelector(panelSel + '[data-panel="backends"]');
      const input = panel?.querySelector('[data-testid="settings-backend-codex-enabled"]') as HTMLInputElement;
      if (input && !input.disabled) {
        input.checked = true;
        input.dispatchEvent(new Event('change', { bubbles: true }));
      }
    }, sel.settingsTabPanel);
    await browser.pause(200);

    // --- Profiles dropdown ---
    const profileOptions = await browser.execute((profileSel: string) => {
      const select = document.querySelector(profileSel) as HTMLSelectElement | null;
      if (!select) return [];
      return Array.from(select.options).map(o => o.value);
    }, sel.profileSelect);
    expect(profileOptions).toContain('default');
    expect(profileOptions).toContain('work');

    // Verify a profile is actually selected (not empty/defaulting to first alphabetically)
    const selectedProfile = await browser.execute((profileSel: string) => {
      const select = document.querySelector(profileSel) as HTMLSelectElement | null;
      return select?.value ?? null;
    }, sel.profileSelect);
    // Assert on the EXACT active profile - should be 'default' for a fresh workspace
    expect(selectedProfile).toBe('default');

    // --- Dynamic module tabs from schemas ---
    const moduleTabExists = await browser.execute((navItemSel: string) => {
      return document.querySelector(navItemSel + '[data-tab="module-execution"]') !== null;
    }, sel.settingsNavItem);
    expect(moduleTabExists).toBe(true);

    // --- Module schema fields ---
    await browser.execute((navItemSel: string) => {
      const btn = document.querySelector(navItemSel + '[data-tab="module-execution"]') as HTMLButtonElement | null;
      btn?.click();
    }, sel.settingsNavItem);
    await browser.pause(300);

    const fieldLabels = await browser.execute((panelSel: string, labelSel: string) => {
      const panel = document.querySelector(panelSel + '[data-panel="module-execution"]');
      if (!panel) return [];
      const labels = panel.querySelectorAll(labelSel);
      return Array.from(labels).map(l => l.textContent);
    }, sel.settingsTabPanel, sel.settingsLabel);
    expect(fieldLabels).toContain('Timeout Seconds');
    expect(fieldLabels).toContain('Sandbox Enabled');
  });
});
