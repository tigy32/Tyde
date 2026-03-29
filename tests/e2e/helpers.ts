export const sel = {
  // App chrome
  app: '#app',
  appTitle: '[data-testid="app-title"]',
  openWorkspaceBtn: '#open-workspace-btn',
  openRemoteBtn: '#open-remote-workspace-btn',

  // Chat
  chatContainer: '[data-testid="chat-container"]',
  messageInput: 'textarea[aria-label="Message input"]',
  sendBtn: '[data-testid="send-btn"]',
  typingIndicator: '[data-testid="typing-indicator"]',
  scrollToBottom: '[data-testid="scroll-to-bottom"]',
  chatMessage: '[data-testid="chat-message"]',
  assistantMessage: '[data-testid="chat-message"].assistant-message',
  userMessage: '[data-testid="chat-message"].user-message',
  systemMessage: '[data-testid="system-message"]',
  streamingMessage: '[data-testid="chat-message"].streaming',

  // Queue & interrupt
  queueIndicator: '[data-testid="queue-indicator"]',
  queueItem: '[data-testid="queue-item"]',
  queueItemText: '[data-testid="queue-item-text"]',
  queueItemSteer: '[data-testid="queue-item-steer"]',
  queueItemRemove: '[data-testid="queue-item-remove"]',
  cancelBtn: '[data-testid="cancel-btn"]',

  // Welcome screen
  welcomeScreen: '[data-testid="welcome-screen"]',
  welcomeNewChat: '[data-testid="welcome-new-chat"]',

  // Tool cards
  toolCard: '[data-testid="tool-card"]',
  embeddedToolCalls: '[data-testid="embedded-tool-calls"]',
  toolStatusText: '[data-testid="tool-card"] .tool-status-text',
  toolCardName: '[data-testid="tool-card"] .tool-name',

  // Error
  errorMessage: '[data-testid="error-message"]',

  // Context bar
  contextUsage: '[data-testid="context-usage"]',
  contextBar: '[data-testid="context-bar"]',
  contextSegment: '[data-testid="context-segment"]',

  // Task panel
  taskPanel: '[data-testid="task-panel"]',

  // Sessions
  sessionsPanel: '[data-testid="sessions-panel"]',
  sessionsRefresh: '[data-testid="sessions-refresh"]',
  sessionsList: '[data-testid="sessions-list"]',
  sessionCard: '[data-testid="session-card"]',

  // Agents
  agentsPanel: '.agents-panel',
  agentCard: '.agent-card',
  agentCardTitle: '.agent-card-title',
  agentCardRunning: '.agent-card.agent-card-running',
  agentCardInterrupt: '[data-testid="agent-card-interrupt"]',
  agentCardTerminate: '[data-testid="agent-card-terminate"]',
  agentCardRemove: '[data-testid="agent-card-remove"]',

  // Tabs
  tabBar: '[data-testid="tab-bar"]',
  convTab: '[data-testid="conv-tab"]',
  convTabActive: '[data-testid="conv-tab"].conv-tab-active',
  convTabTitle: '[data-testid="conv-tab-title"]',

  // Workspace
  workspaceView: '[data-testid="workspace-view"]',

  // Home view
  homeView: '[data-testid="home-view"]',
  homeNewBridgeChat: '[data-testid="home-new-bridge-chat"]',
  homeOpenWorkspace: '[data-testid="home-open-workspace"]',
  homeOpenRemote: '[data-testid="home-open-remote"]',
  projectCard: '[data-testid="project-card"]',
  projectName: '[data-testid="project-name"]',
  projectAgentCount: '[data-testid="project-agent-count"]',
  homeTabBar: '[data-testid="home-tab-bar"]',
  homeTabProjects: '[data-testid="home-tab-projects"]',
  homeTabAgents: '[data-testid="home-tab-agents"]',
  homeAgentsSection: '[data-testid="home-agents-section"]',
  homeAgentCard: '[data-testid="home-agent-card"]',
  headerSettingsBtn: '[data-testid="header-settings-btn"]',
  homeKeyboardHints: '[data-testid="home-keyboard-hints"]',
  homeWizard: '[data-testid="home-wizard"]',
  wizardNext: '[data-testid="wizard-next"]',
  wizardFinish: '[data-testid="wizard-finish"]',
  homeEmptyProjects: '[data-testid="home-empty-projects"]',

  // Git
  gitClean: '[data-testid="git-clean"]',
  gitEmpty: '[data-testid="git-empty"]',
  gitError: '[data-testid="git-error"]',

  // Settings
  settingsPanel: '[data-testid="settings-panel"]',
  settingsClose: '[data-testid="settings-close"]',
  settingsNav: '[data-testid="settings-nav"]',
  settingsTabView: '[data-testid="settings-tab-view"]',
  profileSelect: '[data-testid="profile-select"]',
  settingsNavItem: '[data-testid="settings-nav-item"]',
  settingsTabPanel: '[data-testid="settings-tab-panel"]',
  settingsSelect: '[data-testid="settings-select"]',
  settingsCardName: '[data-testid="settings-card-name"]',
  settingsLabel: '[data-testid="settings-label"]',
  settingsHostSelect: '[data-testid="settings-host-select"]',
  settingsHostAdd: '[data-testid="settings-host-add"]',
  settingsHostRemove: '[data-testid="settings-host-remove"]',

  railHostHeader: '[data-testid="rail-host-header"]',
  railHostAddBtn: '[data-testid="rail-host-add-btn"]',

  remoteBrowserDialog: '[data-testid="remote-browser-dialog"]',
  remoteBrowserPath: '[data-testid="remote-browser-path"]',
  remoteBrowserList: '[data-testid="remote-browser-list"]',
  remoteBrowserRow: '[data-testid="remote-browser-row"]',
  remoteBrowserSelect: '[data-testid="remote-browser-select"]',

  // Notifications
  notificationError: '[data-testid="notification-error"]',

  // Rail
  railAddBtn: '[data-testid="rail-add-btn"]',
  railProjectItem: '[data-testid="rail-project-item"]',
  railWorkbenchItem: '[data-testid="rail-workbench-item"]',
  railProjectName: '[data-testid="rail-project-name"]',
  railHomeItem: '[data-testid="rail-home-item"]',
  railContextNewWorkbench: '[data-testid="rail-context-new-workbench"]',
  railContextRemoveWorkbench: '[data-testid="rail-context-remove-workbench"]',

  // Dock
  leftDockBtn: '[data-testid="left-dock-btn"]',
  rightDockBtn: '[data-testid="right-dock-btn"]',
  dockWidgetTab: '[data-testid="dock-widget-tab"]',
  dockConversationTab: '[data-testid="dock-conversation-tab"]',
  dockedConversation: '[data-testid="docked-conversation"]',
  dockConversationTabActive: '[data-testid="dock-conversation-tab"].dock-widget-tab-active',

  // Text prompt dialog
  textPrompt: '[data-testid="text-prompt"]',
  textPromptInput: '[data-testid="text-prompt-input"]',
  textPromptConfirm: '[data-testid="text-prompt-confirm"]',

  // Connection dialog
  connectionDialog: '[data-testid="connection-dialog"]',
  connectionHost: '[data-testid="connection-host"]',
  connectionStep: (name: string) => `[data-step="${name}"]`,
  connectionClose: '[data-testid="connection-close"]',
};

export async function resetAppState(storageEntries: Record<string, string> = {}): Promise<void> {
  await browser.url('/');
  await browser.execute((entries: Record<string, string>) => {
    window.localStorage.clear();
    const defaults: Record<string, string> = {
      'mock-mcp-http-enabled': 'true',
      'tyde-onboarding-complete': 'true',
    };
    const merged = { ...defaults, ...entries };
    for (const [key, value] of Object.entries(merged)) {
      window.localStorage.setItem(key, value);
    }
  }, storageEntries);
  await browser.url('/');
}

export async function openWorkspace(): Promise<void> {
  await resetAppState();
  const app = await $(sel.app);
  await app.waitForExist({ timeout: 10_000 });

  const openWorkspace = await $(sel.openWorkspaceBtn);
  await openWorkspace.waitForClickable({ timeout: 10_000 });
  await openWorkspace.click();

  const title = await $(sel.appTitle);
  await browser.waitUntil(
    async () => (await title.getText()).includes('workspace'),
    { timeout: 10_000, timeoutMsg: 'Workspace did not load' },
  );
}

export async function openWorkspaceAndWaitForChat(): Promise<void> {
  await openWorkspace();

  await browser.keys(['Control', 'n']);

  const input = await $(sel.messageInput);
  await input.waitForDisplayed({ timeout: 10_000 });
}

export async function openRemoteWorkspaceAndWaitForChat(): Promise<void> {
  await browser.url('/');
  const app = await $(sel.app);
  await app.waitForExist({ timeout: 10_000 });

  await browser.execute(() => {
    (window as any).__mockDialogPath = 'ssh://testuser@remotehost.example.com/home/testuser/project';
  });

  const openWorkspace = await $(sel.openWorkspaceBtn);
  await openWorkspace.waitForClickable({ timeout: 10_000 });
  await openWorkspace.click();

  const title = await $(sel.appTitle);
  await browser.waitUntil(
    async () => (await title.getText()).includes('remotehost'),
    { timeout: 15_000, timeoutMsg: 'Remote workspace did not load' },
  );

  await browser.keys(['Control', 'n']);

  const input = await $(sel.messageInput);
  await input.waitForDisplayed({ timeout: 10_000 });
}

export async function sendPromptAndWaitForAssistant(prompt: string): Promise<void> {
  const countBefore = await browser.execute(
    (s: string) => document.querySelectorAll(s).length,
    sel.assistantMessage,
  );

  const input = await $(sel.messageInput);
  await input.waitForDisplayed({ timeout: 10_000 });
  await browser.execute(
    (el: HTMLElement, text: string) => {
      const inputEl = el as HTMLTextAreaElement;
      inputEl.value = text;
      inputEl.dispatchEvent(new Event('input', { bubbles: true }));
    },
    input,
    prompt,
  );

  const send = await $(sel.sendBtn);
  await send.waitForDisplayed({ timeout: 5_000 });
  await send.waitForEnabled({ timeout: 5_000 });
  await browser.execute((el: HTMLElement) => {
    (el as HTMLButtonElement).click();
  }, send);

  await browser.waitUntil(
    async () => {
      return browser.execute((prevCount: number, s: string) => {
        const messages = Array.from(document.querySelectorAll(s));
        if (messages.length <= prevCount) return false;
        const last = messages[messages.length - 1];
        return !last.classList.contains('streaming');
      }, countBefore, sel.assistantMessage);
    },
    { timeout: 10_000, timeoutMsg: 'Assistant response did not complete' },
  );
}
