import { openWorkspace, resetAppState, sel } from './helpers';

describe('Home screen and app launch', () => {
  it('lets you start a Bridge chat from home when MCP control is enabled', async () => {
    await openWorkspace();

    await browser.waitUntil(
      async () => browser.execute((railSel: string) => {
        const railItem = document.querySelector(railSel) as HTMLElement | null;
        if (!railItem) return false;
        const style = window.getComputedStyle(railItem);
        const rect = railItem.getBoundingClientRect();
        if (style.display === 'none' || style.visibility === 'hidden' || rect.width <= 0 || rect.height <= 0) {
          return false;
        }
        railItem.click();
        return true;
      }, sel.railHomeItem),
      { timeout: 5000, timeoutMsg: 'Expected Home rail item to be clickable' },
    );

    await browser.waitUntil(
      async () => (await $(sel.homeNewBridgeChat)).isDisplayed(),
      { timeout: 5000, timeoutMsg: 'Expected Bridge chat button on home view' },
    );
    expect(await (await $(sel.homeNewBridgeChat)).getText()).toContain('Bridge');
  });

  it('disables Bridge chat creation when MCP control is turned off', async () => {
    await resetAppState({ 'mock-mcp-http-enabled': 'false' });

    await browser.waitUntil(
      async () => (await $(sel.homeNewBridgeChat)).isDisplayed(),
      { timeout: 5000, timeoutMsg: 'Expected Bridge chat button on home view' },
    );
    await browser.waitUntil(
      async () => (await $(sel.homeNewBridgeChat)).getAttribute('disabled') !== null,
      { timeout: 5000, timeoutMsg: 'Expected Bridge chat button to stay disabled when MCP control is off' },
    );
    expect(await (await $(sel.homeNewBridgeChat)).getAttribute('title')).toContain('Enable Loopback MCP Control');
  });

  it('loads the app, shows home view, then opens a workspace with welcome screen', async () => {
    await resetAppState();

    // App title is correct
    const title = await browser.getTitle();
    expect(title).toBe('Tyde');

    // Main layout renders
    const app = await $(sel.app);
    await app.waitForExist({ timeout: 5000 });
    expect(await app.isExisting()).toBe(true);

    // Header actions are present
    await browser.waitUntil(
      async () => (await $(sel.homeNewBridgeChat)).isDisplayed(),
      { timeout: 5000, timeoutMsg: 'Expected Bridge chat action on home view' },
    );
    await browser.waitUntil(
      async () => (await $(sel.openWorkspaceBtn)).isDisplayed(),
      { timeout: 5000, timeoutMsg: 'Expected open workspace action on home view' },
    );
    await browser.waitUntil(
      async () => (await $(sel.openRemoteBtn)).isDisplayed(),
      { timeout: 5000, timeoutMsg: 'Expected open remote action on home view' },
    );
    await browser.waitUntil(
      async () => (await $(sel.leftDockBtn)).isDisplayed(),
      { timeout: 5000, timeoutMsg: 'Expected left dock toggle in the header' },
    );
    await browser.waitUntil(
      async () => (await $(sel.rightDockBtn)).isDisplayed(),
      { timeout: 5000, timeoutMsg: 'Expected right dock toggle in the header' },
    );

    // Home view visible when no workspace is open
    const homeView = await $(sel.homeView);
    await homeView.waitForExist({ timeout: 5000 });
    expect(await homeView.isDisplayed()).toBe(true);

    // Open a workspace
    await openWorkspace();

    // Welcome screen appears in the workspace
    const welcome = await $(sel.welcomeScreen);
    await welcome.waitForExist({ timeout: 5000 });
    expect(await welcome.isDisplayed()).toBe(true);

    // Welcome screen has the new chat button
    const welcomeNewChat = await $(sel.welcomeNewChat);
    expect(await welcomeNewChat.isDisplayed()).toBe(true);
  });

  it('keeps home project agent counts in sync with created chats', async () => {
    await openWorkspace();

    const welcomeNewChat = await $(sel.welcomeNewChat);
    await welcomeNewChat.waitForExist({ timeout: 5000 });
    await browser.execute((el: HTMLElement) => el.click(), welcomeNewChat);

    const input = await $(sel.messageInput);
    await input.waitForDisplayed({ timeout: 5000 });

    await browser.waitUntil(
      async () => browser.execute((railSel: string) => {
        const railItem = document.querySelector(railSel) as HTMLElement | null;
        if (!railItem) return false;
        const style = window.getComputedStyle(railItem);
        const rect = railItem.getBoundingClientRect();
        if (style.display === 'none' || style.visibility === 'hidden' || rect.width <= 0 || rect.height <= 0) {
          return false;
        }
        railItem.click();
        return true;
      }, sel.railHomeItem),
      { timeout: 5000, timeoutMsg: 'Expected Home rail item to be clickable' },
    );

    const agentCount = await $(sel.projectAgentCount);
    await browser.waitUntil(
      async () => {
        const text = await agentCount.getText();
        return text.includes('1 total') || text.includes('1 active agent');
      },
      { timeout: 5000, timeoutMsg: 'Expected home project card count to include one agent after creating chat' },
    );

  });

  it('shows all agents across workspaces in the Agents tab including MCP-spawned agents', async () => {
    await resetAppState();

    // Tab bar is present on home view
    const tabBar = await $(sel.homeTabBar);
    await tabBar.waitForExist({ timeout: 5000 });
    expect(await tabBar.isDisplayed()).toBe(true);

    // Projects tab is active by default
    await browser.waitUntil(
      async () => (await $(sel.homeTabProjects)).isDisplayed(),
      { timeout: 5000, timeoutMsg: 'Expected home Projects tab to be visible' },
    );
    await browser.waitUntil(
      async () => (await $(sel.homeTabAgents)).isDisplayed(),
      { timeout: 5000, timeoutMsg: 'Expected home Agents tab to be visible' },
    );

    // Switch to Agents tab — empty state with no agents
    await browser.waitUntil(
      async () => browser.execute((tabSel: string, sectionSel: string) => {
        const tab = document.querySelector(tabSel) as HTMLElement | null;
        tab?.click();
        const section = document.querySelector(sectionSel) as HTMLElement | null;
        if (!section) return false;
        const style = window.getComputedStyle(section);
        const rect = section.getBoundingClientRect();
        return style.display !== 'none'
          && style.visibility !== 'hidden'
          && rect.width > 0
          && rect.height > 0;
      }, sel.homeTabAgents, sel.homeAgentsSection),
      { timeout: 5000, timeoutMsg: 'Expected home Agents section to be visible after selecting Agents tab' },
    );

    // Empty state text should appear
    await browser.waitUntil(
      async () => {
        const text = await browser.execute((sectionSel: string) => {
          const section = document.querySelector(sectionSel);
          return section?.textContent ?? '';
        }, sel.homeAgentsSection);
        return text.includes('No agents running');
      },
      { timeout: 5000, timeoutMsg: 'Expected empty agents state text' },
    );

    // Spawn an MCP-style agent via the mock backend
    await browser.execute(() => {
      return (window as any).__TAURI_INTERNALS__.invoke('spawn_agent', {
        workspaceRoots: ['/mock/workspace'],
        prompt: 'Test MCP agent task',
        name: 'MCP Test Agent',
        keepAliveWithoutTab: true,
      });
    });

    // Wait for the mock agent to complete
    await browser.pause(200);

    // Switch to Projects tab then back to Agents to force a fresh fetch
    await browser.execute((tabSel: string) => {
      const tab = document.querySelector(tabSel) as HTMLElement | null;
      tab?.click();
    }, sel.homeTabProjects);
    await browser.pause(50);
    await browser.execute((tabSel: string) => {
      const tab = document.querySelector(tabSel) as HTMLElement | null;
      tab?.click();
    }, sel.homeTabAgents);

    // Agent card should appear
    await browser.waitUntil(
      async () => {
        const cards = await $$(sel.homeAgentCard);
        return (await cards.length) >= 1;
      },
      { timeout: 5000, timeoutMsg: 'Expected at least one agent card after spawning MCP agent' },
    );

    // Verify agent card displays the correct name
    const card = await $(sel.homeAgentCard);
    const cardText = await card.getText();
    expect(cardText).toContain('MCP Test Agent');
  });

  it('does not expose git or files widgets in the home bridge window', async () => {
    await resetAppState();

    const bridgeBtn = await $(sel.homeNewBridgeChat);
    await bridgeBtn.waitForExist({ timeout: 5000 });
    await browser.waitUntil(
      async () => (await bridgeBtn.getAttribute('disabled')) === null,
      { timeout: 5000, timeoutMsg: 'Expected Bridge chat button to enable when MCP control is on' },
    );
    await bridgeBtn.click();

    const input = await $(sel.messageInput);
    await input.waitForDisplayed({ timeout: 5000 });

    await browser.waitUntil(
      async () => browser.execute(() => {
        return Array.from(document.querySelectorAll('.dock-zone'))
          .filter((el) => !(el as HTMLElement).classList.contains('dock-zone-hidden'))
          .length >= 2;
      }),
      { timeout: 5000, timeoutMsg: 'Expected the Bridge window to keep the left and right docks available' },
    );

    const bottomDock = await $('#bottom-dock-btn');
    await bottomDock.waitForClickable({ timeout: 5000 });
    await bottomDock.click();

    await browser.waitUntil(
      async () => browser.execute(() => {
        return Array.from(document.querySelectorAll('.dock-zone'))
          .filter((el) => !(el as HTMLElement).classList.contains('dock-zone-hidden'))
          .length >= 3;
      }),
      { timeout: 5000, timeoutMsg: 'Expected left and bottom docks to open in the Bridge window' },
    );

    const widgetTitles = await browser.execute((tabSel: string) => {
      return Array.from(document.querySelectorAll(tabSel))
        .filter((el) => (el as HTMLElement).offsetParent !== null)
        .map((el) => el.textContent?.trim() ?? '')
        .filter(Boolean);
    }, sel.dockWidgetTab);

    expect(widgetTitles).not.toContain('Files');
    expect(widgetTitles).not.toContain('Git');
  });

  it('shows MCP-spawned runtime agents in the home dock Agents widget', async () => {
    await openWorkspace();

    await browser.execute(() => {
      return (window as any).__TAURI_INTERNALS__.invoke('spawn_agent', {
        workspaceRoots: ['/mock/workspace'],
        prompt: 'Test home dock agent card',
        name: 'Home Dock MCP Agent',
        keepAliveWithoutTab: true,
        mockCompletionDelayMs: 5000,
      });
    });

    await browser.waitUntil(
      async () => browser.execute((railSel: string) => {
        const railItem = document.querySelector(railSel) as HTMLElement | null;
        if (!railItem) return false;
        const style = window.getComputedStyle(railItem);
        const rect = railItem.getBoundingClientRect();
        if (style.display === 'none' || style.visibility === 'hidden' || rect.width <= 0 || rect.height <= 0) {
          return false;
        }
        railItem.click();
        return true;
      }, sel.railHomeItem),
      { timeout: 5000, timeoutMsg: 'Expected Home rail item to be clickable' },
    );

    const bridgeBtn = await $(sel.homeNewBridgeChat);
    await bridgeBtn.waitForExist({ timeout: 5000 });
    await browser.waitUntil(
      async () => (await bridgeBtn.getAttribute('disabled')) === null,
      { timeout: 5000, timeoutMsg: 'Expected Bridge chat button to enable when MCP control is on' },
    );
    await bridgeBtn.click();

    const input = await $(sel.messageInput);
    await input.waitForDisplayed({ timeout: 5000 });

    await browser.execute((tabSel: string) => {
      const tab = Array.from(document.querySelectorAll(tabSel))
        .find((el) => (el as HTMLElement).offsetParent !== null && (el.textContent ?? '').trim() === 'Agents') as HTMLElement | undefined;
      tab?.click();
    }, sel.dockWidgetTab);

    await browser.waitUntil(
      async () => browser.execute((titleSel: string) => {
        return Array.from(document.querySelectorAll(titleSel))
          .filter((el) => (el as HTMLElement).offsetParent !== null)
          .map((el) => el.textContent?.trim() ?? '')
          .includes('Home Dock MCP Agent');
      }, sel.agentCardTitle),
      { timeout: 5000, timeoutMsg: 'Expected MCP runtime agent to appear in the home dock Agents widget' },
    );
  });

  it('hides internal title helper agents from home agent surfaces', async () => {
    await openWorkspace();

    await browser.execute(() => {
      return (window as any).__TAURI_INTERNALS__.invoke('spawn_agent', {
        workspaceRoots: ['/mock/workspace'],
        prompt: 'Internal title helper',
        name: '__internal_title__9001',
        keepAliveWithoutTab: true,
        mockCompletionDelayMs: 5000,
      });
    });

    const homeRail = await $(sel.railHomeItem);
    await homeRail.waitForClickable({ timeout: 5000 });
    await homeRail.click();

    await browser.waitUntil(
      async () => browser.execute((tabSel: string) => {
        const tab = document.querySelector(tabSel) as HTMLElement | null;
        if (!tab) return false;
        const style = window.getComputedStyle(tab);
        const rect = tab.getBoundingClientRect();
        if (style.display === 'none' || style.visibility === 'hidden' || rect.width <= 0 || rect.height <= 0) {
          return false;
        }
        tab.click();
        return true;
      }, sel.homeTabAgents),
      { timeout: 5000, timeoutMsg: 'Expected home Agents tab to be clickable' },
    );

    await browser.waitUntil(
      async () => browser.execute((cardSel: string) => {
        return !Array.from(document.querySelectorAll(cardSel))
          .filter((el) => (el as HTMLElement).offsetParent !== null)
          .map((el) => el.textContent ?? '')
          .some((text) => text.includes('__internal_title__9001'));
      }, sel.homeAgentCard),
      { timeout: 5000, timeoutMsg: 'Expected internal title helpers to be hidden from home Agents tab' },
    );

    const bridgeBtn = await $(sel.homeNewBridgeChat);
    await bridgeBtn.waitForExist({ timeout: 5000 });
    await browser.waitUntil(
      async () => (await bridgeBtn.getAttribute('disabled')) === null,
      { timeout: 5000, timeoutMsg: 'Expected Bridge chat button to enable when MCP control is on' },
    );
    await bridgeBtn.click();

    const input = await $(sel.messageInput);
    await input.waitForDisplayed({ timeout: 5000 });

    await browser.execute((tabSel: string) => {
      const tab = Array.from(document.querySelectorAll(tabSel))
        .find((el) => (el as HTMLElement).offsetParent !== null && (el.textContent ?? '').trim() === 'Agents') as HTMLElement | undefined;
      tab?.click();
    }, sel.dockWidgetTab);

    await browser.waitUntil(
      async () => browser.execute((titleSel: string) => {
        return !Array.from(document.querySelectorAll(titleSel))
          .filter((el) => (el as HTMLElement).offsetParent !== null)
          .map((el) => el.textContent?.trim() ?? '')
          .some((text) => text.includes('__internal_title__9001'));
      }, sel.agentCardTitle),
      { timeout: 5000, timeoutMsg: 'Expected internal title helpers to be hidden from home dock Agents widget' },
    );
  });
});
