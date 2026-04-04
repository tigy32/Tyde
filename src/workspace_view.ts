import type { SessionMetadata } from "@tyde/protocol";
import type { AgentDefinitionStore } from "./agent_defs/store";
import type { AgentDefinitionEntry } from "./agent_defs/types";
import { type AgentCardAction, type AgentInfo, AgentsPanel } from "./agents";
import type {
  AdminEventPayload,
  BackendKind,
  ChatEventPayload,
  Host,
  RuntimeAgent,
} from "./bridge";
import {
  adminDeleteSession,
  adminListSessions,
  cancelConversation,
  closeAdminSubprocess,
  closeConversation,
  collectAgentResult,
  createAdminSubprocess,
  createConversation,
  deleteSessionRecord,
  listHosts,
  listSessionRecords,
  normalizeBackendKind,
  onFileChanged,
  readFileContent,
  renameAgent,
  renameSession,
  resumeSession,
  sendMessage,
  setSessionAlias,
  spawnAgent,
  switchProfile,
  syncFileWatchPaths,
  terminateAgent,
  unwatchWorkspaceDir,
  waitForAgent,
  watchWorkspaceDir,
} from "./bridge";
import { ChatPanel } from "./chat";
import { createToolOutputToggleButton } from "./chat/tools";
import { DiffPanel } from "./diff_panel";
import { EventRouter } from "./event_router";
import { FileExplorer } from "./explorer";
import { GitPanel } from "./git";
import { formatShortcut } from "./keyboard";
import { Layout, type PersistentWidgetId } from "./layout";
import type { NotificationManager } from "./notifications";
import { logTabPerf, perfNow } from "./perf_debug";
import { SessionsPanel } from "./sessions";
import { getDefaultSpawnProfile } from "./settings";
import type { TabState } from "./tabs";
import { TabManager } from "./tabs";
import { TerminalService } from "./terminal";
import type { PanelType } from "./tiling/types";
import { WorkflowBuilder } from "./workflows/builder";
import { WorkflowEngine } from "./workflows/engine";
import { WorkflowsPanel } from "./workflows/panel";
import { WorkflowStore } from "./workflows/store";
import { parseRemoteWorkspaceUri } from "./workspace";

interface WorkspaceViewConfig {
  projectId: string;
  workspacePath: string;
  projectName: string;
  notifications: NotificationManager;
  mode?: "workspace" | "bridge" | "orchestrator";
  roots?: string[];
  agentDefinitionId?: string;
  agentDefinitionStore?: AgentDefinitionStore;
  getBridgeProjects?:
    | (() => Array<{ name: string; workspacePath: string; roots: string[] }>)
    | null;
  bridgeChatEnabled?: boolean;
  bridgeChatDisabledReason?: string;
  availableWidgets?: PersistentWidgetId[];
}

interface SessionsListWaiter {
  resolve: () => void;
  reject: (reason?: unknown) => void;
  timeoutId: ReturnType<typeof setTimeout>;
}

const SESSIONS_LIST_WAIT_TIMEOUT_MS = 10_000;
const INTERNAL_TITLE_AGENT_PREFIX = "__internal_title__";

function basename(path: string): string {
  const parts = path.replace(/\\/g, "/").split("/");
  return parts[parts.length - 1] || path;
}

function isConversationMissingError(err: unknown): boolean {
  const msg = String(err ?? "").toLowerCase();
  return (
    msg.includes("conversation not found") ||
    msg.includes("no active conversation") ||
    msg.includes("not found in state")
  );
}

export class WorkspaceView {
  readonly projectId: string;
  readonly workspacePath: string;
  readonly root: HTMLElement;

  private readonly mode: "workspace" | "bridge" | "orchestrator";
  private roots: string[];
  private readonly agentDefinitionId: string | undefined;
  private definitionLabel: string;
  private readonly agentDefinitionStore: AgentDefinitionStore | null;
  private readonly getBridgeProjects: () => Array<{
    name: string;
    workspacePath: string;
    roots: string[];
  }>;
  private chatPanel: ChatPanel;
  private tabManager: TabManager;
  private layout: Layout;
  private gitPanel: GitPanel;
  private fileExplorer: FileExplorer;
  private diffPanel: DiffPanel;
  private sessionsPanel: SessionsPanel;
  private agentsPanel: AgentsPanel;
  private terminalService: TerminalService;
  private workflowStore: WorkflowStore;
  private workflowEngine: WorkflowEngine;
  private workflowsPanel: WorkflowsPanel;
  private workflowBuilder: WorkflowBuilder;
  private eventRouter: EventRouter;
  private notifications: NotificationManager;
  private conversationIds: Set<number> = new Set();
  private fileChangeUnlisten: (() => void) | null = null;
  private watchedFilePaths = new Set<string>();
  private fileRefreshInFlight = new Set<string>();
  private explorerRefreshTimer: ReturnType<typeof setTimeout> | null = null;

  private adminIds: Partial<Record<BackendKind, number>> = {};
  private adminBackendById = new Map<number, BackendKind>();
  private sessionsByBackend = new Map<BackendKind, SessionMetadata[]>();
  private sessionsListWaitersByBackend = new Map<
    BackendKind,
    Set<SessionsListWaiter>
  >();
  private conversationSessionMap = new Map<
    number,
    { sessionId: string; backendKind: BackendKind }
  >();
  private conversationBackendKindMap = new Map<number, BackendKind>();
  private conversationTydeSessionMap = new Map<number, string>();
  private titleGenerationRequested = new Set<number>();
  private sessionsRefreshRequestedForConversation = new Set<number>();
  private _readyPromise: Promise<void> | null = null;
  private _constructorWork: Promise<unknown>[] = [];
  private uiCleanupFns: Array<() => void> = [];
  private newConversationEnabled = true;
  private newConversationDisabledReason = "New conversations are unavailable.";
  private homeViewContainer: HTMLElement | null = null;
  private centerNewTabBtn: HTMLButtonElement | null = null;
  private centerNewTabMenuBtn: HTMLButtonElement | null = null;
  private centerNewTabMenu: HTMLDivElement | null = null;
  private centerNewTabMenuItems: Partial<
    Record<BackendKind, HTMLButtonElement>
  > = {};
  private hostEnabledBackends: BackendKind[] | null = null;
  private hostDefaultBackend: BackendKind | null = null;
  private resolvedHostId: string | null = null;
  private hostSettingsReady: Promise<void> = Promise.resolve();
  onConversationIdsChange: ((conversationIds: number[]) => void) | null = null;
  onAgentsChange: ((agents: AgentInfo[]) => void) | null = null;
  onRuntimeAgentAction:
    | ((agent: AgentInfo, action: AgentCardAction) => void)
    | null = null;
  onRuntimeAgentClick: ((agent: AgentInfo) => void) | null = null;
  onWorkflowsChanged: (() => void) | null = null;
  private serverConnStatusEl: HTMLElement | null = null;

  constructor(config: WorkspaceViewConfig) {
    this.projectId = config.projectId;
    this.workspacePath = config.workspacePath;
    this.notifications = config.notifications;
    this.mode = config.mode ?? "workspace";
    this.roots = config.roots ?? [];
    this.agentDefinitionId = config.agentDefinitionId;
    this.agentDefinitionStore = config.agentDefinitionStore ?? null;
    this.definitionLabel = this.resolveDefinitionLabel();
    if (this.agentDefinitionStore) {
      this.uiCleanupFns.push(
        this.agentDefinitionStore.subscribe(() => {
          this.refreshDefinitionLabel();
          this.updateNewConversationAvailability(
            this.newConversationEnabled,
            this.newConversationDisabledReason,
          );
        }),
      );
    }
    this.getBridgeProjects = config.getBridgeProjects ?? (() => []);
    this.newConversationEnabled = config.bridgeChatEnabled ?? true;
    if (
      typeof config.bridgeChatDisabledReason === "string" &&
      config.bridgeChatDisabledReason.trim().length > 0
    ) {
      this.newConversationDisabledReason = config.bridgeChatDisabledReason;
    }
    void this.refreshHostSettings();

    this.root = document.createElement("div");
    this.root.className = "workspace-view";
    this.root.style.display = "none";
    this.root.dataset.testid = "workspace-view";
    this.root.dataset.viewMode = this.mode;

    const chatContainer = document.createElement("div");
    chatContainer.className = "chat-shell";
    chatContainer.style.display = "flex";
    chatContainer.style.flexDirection = "column";
    chatContainer.style.height = "100%";
    chatContainer.style.overflow = "hidden";
    chatContainer.style.minHeight = "0";

    const tabBarEl = document.createElement("div");
    tabBarEl.className = "conv-tab-bar";
    tabBarEl.dataset.testid = "tab-bar";

    const centerNewTabSplit = document.createElement("div");
    centerNewTabSplit.className = "center-tab-new-split";

    const centerNewTabBtn = document.createElement("button");
    centerNewTabBtn.id = "center-new-tab-btn";
    centerNewTabBtn.className = "center-tab-new-btn center-tab-new-btn-primary";
    centerNewTabBtn.type = "button";
    centerNewTabBtn.textContent = "+";
    centerNewTabBtn.title =
      this.agentDefinitionId != null
        ? `New ${this.definitionLabel} tab (${formatShortcut("Ctrl+N")})`
        : `New tab (${formatShortcut("Ctrl+N")})`;
    centerNewTabBtn.setAttribute(
      "aria-label",
      this.agentDefinitionId != null
        ? `New ${this.definitionLabel} chat`
        : "New chat",
    );

    const centerNewTabMenuBtn = document.createElement("button");
    centerNewTabMenuBtn.className =
      "center-tab-new-btn center-tab-new-btn-menu";
    centerNewTabMenuBtn.type = "button";
    centerNewTabMenuBtn.textContent = "v";
    centerNewTabMenuBtn.title =
      this.agentDefinitionId != null
        ? `New ${this.definitionLabel} chat options`
        : "New chat options";
    centerNewTabMenuBtn.setAttribute(
      "aria-label",
      this.agentDefinitionId != null
        ? `Choose backend for new ${this.definitionLabel} chat`
        : "Choose backend for new chat",
    );
    centerNewTabMenuBtn.setAttribute("aria-haspopup", "menu");
    centerNewTabMenuBtn.setAttribute("aria-expanded", "false");
    centerNewTabMenuBtn.dataset.testid = "center-new-tab-menu-btn";

    const centerNewTabMenu = document.createElement("div");
    centerNewTabMenu.className = "center-tab-new-menu";
    centerNewTabMenu.hidden = true;
    centerNewTabMenu.setAttribute("role", "menu");
    centerNewTabMenu.dataset.testid = "center-new-tab-menu";

    const tycodeMenuItem = document.createElement("button");
    tycodeMenuItem.type = "button";
    tycodeMenuItem.className = "center-tab-new-menu-item";
    tycodeMenuItem.textContent =
      this.agentDefinitionId != null
        ? `New Tycode ${this.definitionLabel}`
        : "New Tycode Chat";
    tycodeMenuItem.setAttribute("role", "menuitem");
    tycodeMenuItem.dataset.testid = "center-new-tab-tycode";

    const codexMenuItem = document.createElement("button");
    codexMenuItem.type = "button";
    codexMenuItem.className = "center-tab-new-menu-item";
    codexMenuItem.textContent =
      this.agentDefinitionId != null
        ? `New Codex ${this.definitionLabel}`
        : "New Codex Chat";
    codexMenuItem.setAttribute("role", "menuitem");
    codexMenuItem.dataset.testid = "center-new-tab-codex";

    const claudeMenuItem = document.createElement("button");
    claudeMenuItem.type = "button";
    claudeMenuItem.className = "center-tab-new-menu-item";
    claudeMenuItem.textContent =
      this.agentDefinitionId != null
        ? `New Claude ${this.definitionLabel}`
        : "New Claude Chat";
    claudeMenuItem.setAttribute("role", "menuitem");
    claudeMenuItem.dataset.testid = "center-new-tab-claude";

    const kiroMenuItem = document.createElement("button");
    kiroMenuItem.type = "button";
    kiroMenuItem.className = "center-tab-new-menu-item";
    kiroMenuItem.textContent =
      this.agentDefinitionId != null
        ? `New Kiro ${this.definitionLabel}`
        : "New Kiro Chat";
    kiroMenuItem.setAttribute("role", "menuitem");
    kiroMenuItem.dataset.testid = "center-new-tab-kiro";

    const geminiMenuItem = document.createElement("button");
    geminiMenuItem.type = "button";
    geminiMenuItem.className = "center-tab-new-menu-item";
    geminiMenuItem.textContent =
      this.agentDefinitionId != null
        ? `New Gemini ${this.definitionLabel}`
        : "New Gemini Chat";
    geminiMenuItem.setAttribute("role", "menuitem");
    geminiMenuItem.dataset.testid = "center-new-tab-gemini";

    const menuItems: { kind: BackendKind; el: HTMLButtonElement }[] = [
      { kind: "tycode", el: tycodeMenuItem },
      { kind: "codex", el: codexMenuItem },
      { kind: "claude", el: claudeMenuItem },
      { kind: "kiro", el: kiroMenuItem },
      { kind: "gemini", el: geminiMenuItem },
    ];
    const enabledSet = new Set(this.getWorkspaceEnabledBackends());
    if (enabledSet.size === 0) {
      const hint = document.createElement("div");
      hint.className = "center-tab-new-menu-empty";
      hint.textContent =
        "No backends enabled. Enable at least one in Settings → Backends.";
      centerNewTabMenu.appendChild(hint);
    }
    for (const { kind, el: item } of menuItems) {
      if (enabledSet.has(kind)) {
        centerNewTabMenu.appendChild(item);
      }
    }
    centerNewTabSplit.appendChild(centerNewTabBtn);
    centerNewTabSplit.appendChild(centerNewTabMenuBtn);

    // Agent definition submenu
    const centerNewTabSubmenu = document.createElement("div");
    centerNewTabSubmenu.className =
      "center-tab-new-menu center-tab-new-submenu";
    centerNewTabSubmenu.hidden = true;
    centerNewTabSubmenu.setAttribute("role", "menu");
    centerNewTabSubmenu.dataset.testid = "center-new-tab-submenu";

    let submenuDismissTimer: ReturnType<typeof setTimeout> | null = null;

    const hideSubmenu = (): void => {
      submenuDismissTimer = setTimeout(() => {
        centerNewTabSubmenu.hidden = true;
      }, 150);
    };

    const cancelHideSubmenu = (): void => {
      if (submenuDismissTimer) {
        clearTimeout(submenuDismissTimer);
        submenuDismissTimer = null;
      }
    };

    const showSubmenu = (
      triggerItem: HTMLButtonElement,
      backendKind: BackendKind,
    ): void => {
      cancelHideSubmenu();
      const definitions = this.agentDefinitionStore?.getAll() ?? [];
      if (definitions.length === 0) {
        centerNewTabSubmenu.hidden = true;
        return;
      }
      centerNewTabSubmenu.innerHTML = "";
      for (const def of definitions) {
        const defItem = document.createElement("button");
        defItem.type = "button";
        defItem.className = "center-tab-new-menu-item";
        defItem.textContent = def.name;
        defItem.setAttribute("role", "menuitem");
        defItem.addEventListener("click", () => {
          closeCenterNewTabMenu();
          this.startNewConversation(undefined, backendKind, def.id);
        });
        centerNewTabSubmenu.appendChild(defItem);
      }
      if (!centerNewTabSubmenu.isConnected) {
        document.body.appendChild(centerNewTabSubmenu);
      }
      centerNewTabSubmenu.style.visibility = "hidden";
      centerNewTabSubmenu.hidden = false;
      const itemRect = triggerItem.getBoundingClientRect();
      const submenuRect = centerNewTabSubmenu.getBoundingClientRect();
      const margin = 8;
      let left = itemRect.right + 2;
      if (left + submenuRect.width > window.innerWidth - margin) {
        left = itemRect.left - submenuRect.width - 2;
      }
      left = Math.max(
        margin,
        Math.min(left, window.innerWidth - submenuRect.width - margin),
      );
      let top = itemRect.top;
      top = Math.max(
        margin,
        Math.min(top, window.innerHeight - submenuRect.height - margin),
      );
      centerNewTabSubmenu.style.left = `${Math.round(left)}px`;
      centerNewTabSubmenu.style.top = `${Math.round(top)}px`;
      centerNewTabSubmenu.style.visibility = "";
    };

    centerNewTabSubmenu.addEventListener("mouseenter", () =>
      cancelHideSubmenu(),
    );
    centerNewTabSubmenu.addEventListener("mouseleave", () => hideSubmenu());

    const positionCenterNewTabMenu = (): void => {
      const triggerRect = centerNewTabSplit.getBoundingClientRect();
      const menuRect = centerNewTabMenu.getBoundingClientRect();
      const margin = 8;
      const preferredLeft = triggerRect.right - menuRect.width;
      const clampedLeft = Math.max(
        margin,
        Math.min(preferredLeft, window.innerWidth - menuRect.width - margin),
      );
      const preferredTop = triggerRect.bottom + 6;
      const clampedTop = Math.max(
        margin,
        Math.min(preferredTop, window.innerHeight - menuRect.height - margin),
      );
      centerNewTabMenu.style.left = `${Math.round(clampedLeft)}px`;
      centerNewTabMenu.style.top = `${Math.round(clampedTop)}px`;
    };

    const closeCenterNewTabMenu = (): void => {
      if (centerNewTabMenu.hidden) return;
      centerNewTabMenu.hidden = true;
      centerNewTabSubmenu.hidden = true;
      cancelHideSubmenu();
      centerNewTabMenuBtn.setAttribute("aria-expanded", "false");
      centerNewTabSplit.classList.remove("open");
    };

    const openCenterNewTabMenu = (): void => {
      if (!this.newConversationEnabled) return;
      if (!centerNewTabMenu.hidden) return;
      if (!centerNewTabMenu.isConnected) {
        document.body.appendChild(centerNewTabMenu);
      }
      const hasDefs = (this.agentDefinitionStore?.getAll().length ?? 0) > 0;
      for (const item of centerNewTabMenu.querySelectorAll(
        ".center-tab-new-menu-item",
      )) {
        item.classList.toggle("center-tab-new-menu-item-has-submenu", hasDefs);
      }
      centerNewTabMenu.style.visibility = "hidden";
      centerNewTabMenu.hidden = false;
      positionCenterNewTabMenu();
      centerNewTabMenu.style.visibility = "";
      centerNewTabMenuBtn.setAttribute("aria-expanded", "true");
      centerNewTabSplit.classList.add("open");
    };

    const chatPanelEl = document.createElement("div");
    chatPanelEl.className = "chat-panel";
    chatPanelEl.style.display = "flex";
    chatPanelEl.style.flexDirection = "column";
    chatPanelEl.style.flex = "1";
    chatPanelEl.style.overflow = "hidden";
    chatPanelEl.style.minHeight = "0";

    const chatTabViewEl = document.createElement("div");
    chatTabViewEl.className = "chat-tab-view";

    chatPanelEl.appendChild(chatTabViewEl);
    chatContainer.appendChild(chatPanelEl);

    const gitPanelEl = document.createElement("div");
    gitPanelEl.style.display = "flex";
    gitPanelEl.style.flexDirection = "column";
    gitPanelEl.style.minHeight = "0";
    gitPanelEl.style.minWidth = "0";
    gitPanelEl.style.height = "100%";
    gitPanelEl.style.overflow = "auto";

    const filesPanelEl = document.createElement("div");
    filesPanelEl.style.display = "flex";
    filesPanelEl.style.flexDirection = "column";
    filesPanelEl.style.minHeight = "0";
    filesPanelEl.style.minWidth = "0";
    filesPanelEl.style.height = "100%";
    filesPanelEl.style.overflow = "auto";

    const diffPanelEl = document.createElement("div");
    diffPanelEl.style.display = "flex";
    diffPanelEl.style.flexDirection = "column";
    diffPanelEl.style.minHeight = "0";
    diffPanelEl.style.minWidth = "0";
    diffPanelEl.style.height = "100%";
    diffPanelEl.style.overflow = "hidden";

    const sessionsContainer = document.createElement("div");
    sessionsContainer.style.display = "flex";
    sessionsContainer.style.flexDirection = "column";
    sessionsContainer.style.minHeight = "0";
    sessionsContainer.style.height = "100%";

    const agentsContainer = document.createElement("div");
    agentsContainer.style.display = "flex";
    agentsContainer.style.flexDirection = "column";
    agentsContainer.style.minHeight = "0";
    agentsContainer.style.height = "100%";

    const terminalContainer = document.createElement("div");
    terminalContainer.className = "terminal-widget-empty";
    terminalContainer.innerHTML =
      "<span>Click + in the bottom dock to open a terminal.</span>";

    const workflowsContainer = document.createElement("div");
    workflowsContainer.style.display = "flex";
    workflowsContainer.style.flexDirection = "column";
    workflowsContainer.style.minHeight = "0";
    workflowsContainer.style.height = "100%";
    workflowsContainer.style.overflow = "auto";

    const panelElements = new Map<PanelType, HTMLElement>();
    panelElements.set("chat", chatContainer);
    panelElements.set("git", gitPanelEl);
    panelElements.set("explorer", filesPanelEl);
    panelElements.set("diff", diffPanelEl);
    panelElements.set("sessions", sessionsContainer);
    panelElements.set("agents", agentsContainer);
    panelElements.set("terminal", terminalContainer);
    panelElements.set("workflows", workflowsContainer);

    const panelFactory = (panelType: PanelType): HTMLElement => {
      return panelElements.get(panelType) ?? document.createElement("div");
    };

    this.layout = new Layout(this.root, panelFactory, config.workspacePath, {
      availableWidgets: config.availableWidgets,
    });
    this.homeViewContainer = this.layout.getHomeViewEl();
    this.homeViewContainer.classList.add("workspace-home-view");

    this.chatPanel = new ChatPanel(chatTabViewEl);
    this.tabManager = new TabManager(tabBarEl);
    this.tabManager.setShowNewTabButton(false);
    this.gitPanel = new GitPanel(gitPanelEl);
    this.fileExplorer = new FileExplorer(filesPanelEl);
    this.diffPanel = new DiffPanel(diffPanelEl);
    this.sessionsPanel = new SessionsPanel(sessionsContainer);
    this.sessionsPanel.setNewSessionAvailability(
      this.newConversationEnabled,
      this.newConversationDisabledReason,
    );
    this.agentsPanel = new AgentsPanel(agentsContainer);
    this.agentsPanel.onChange = (agents) => {
      this.onAgentsChange?.(agents);
    };
    this.agentsPanel.onAgentAction = (agent, action) => {
      if (agent.agentId) {
        this.onRuntimeAgentAction?.(agent, action);
        return;
      }
      void this.handleConversationAgentAction(agent, action);
    };
    this.workflowStore = new WorkflowStore(config.workspacePath);
    this.workflowEngine = new WorkflowEngine(
      config.workspacePath,
      config.workspacePath ? this.getEffectiveRoots() : [],
      this.workflowStore,
    );
    this.workflowsPanel = new WorkflowsPanel(
      workflowsContainer,
      this.workflowStore,
      this.workflowEngine,
    );
    this.workflowBuilder = new WorkflowBuilder(
      this.workflowStore,
      config.workspacePath,
    );
    this.workflowBuilder.onClose = () => {
      this.workflowStore.load().then(() => {
        this.workflowsPanel.render();
        this.onWorkflowsChanged?.();
      });
    };
    this.workflowsPanel.onNewWorkflow = () => this.workflowBuilder.show();
    this.workflowsPanel.onEditWorkflow = (w) => this.workflowBuilder.show(w);
    this.workflowsPanel.onManageWorkflows = () =>
      this.workflowBuilder.showManager();
    this.workflowsPanel.onOpenAgentConversation = (conversationId, name) =>
      this.focusConversation(conversationId, name);
    this.workflowStore.onChange = () => {
      this.workflowsPanel.render();
      this.onWorkflowsChanged?.();
    };

    this.terminalService = new TerminalService(config.workspacePath);

    this.layout.onCreateTerminal = (zone) => {
      void this.createDockedTerminal(zone);
    };
    this.layout.onCloseTerminal = (terminalId) => {
      void this.closeDockedTerminal(terminalId);
    };
    this.layout.onActivateTerminal = (terminalId) => {
      this.terminalService.focus(terminalId);
    };
    this.terminalService.onTitleChange = (terminalId, title) => {
      this.layout.updateDockedTerminalTitle(terminalId, title);
    };
    this.terminalService.onExit = (terminalId) => {
      this.layout.markDockedTerminalExited(terminalId);
    };

    this.layout.setCenterTabBars(tabBarEl);
    this.layout.registerChatTabBar(tabBarEl);
    const tabActionsWrap = document.createElement("div");
    tabActionsWrap.style.display = "flex";
    tabActionsWrap.style.alignItems = "center";
    tabActionsWrap.style.gap = "4px";
    // Connection status indicator for TydeServer remotes
    const connStatus = document.createElement("div");
    connStatus.className = "tyde-server-conn-status";
    connStatus.style.display = "none";
    connStatus.dataset.testid = "tyde-server-conn-status";
    this.serverConnStatusEl = connStatus;
    tabActionsWrap.appendChild(connStatus);

    tabActionsWrap.appendChild(createToolOutputToggleButton());
    tabActionsWrap.appendChild(centerNewTabSplit);
    this.layout.setCenterTabActions(tabActionsWrap);
    centerNewTabBtn.addEventListener("click", () => {
      if (!this.newConversationEnabled) return;
      closeCenterNewTabMenu();
      this.tabManager.onNewTab?.();
    });
    centerNewTabMenuBtn.addEventListener("click", (event) => {
      event.stopPropagation();
      if (!this.newConversationEnabled) return;
      if (centerNewTabMenu.hidden) {
        openCenterNewTabMenu();
      } else {
        closeCenterNewTabMenu();
      }
    });
    tycodeMenuItem.addEventListener("click", () => {
      closeCenterNewTabMenu();
      this.startNewConversation(undefined, "tycode");
    });
    codexMenuItem.addEventListener("click", () => {
      closeCenterNewTabMenu();
      this.startNewConversation(undefined, "codex");
    });
    claudeMenuItem.addEventListener("click", () => {
      closeCenterNewTabMenu();
      this.startNewConversation(undefined, "claude");
    });
    kiroMenuItem.addEventListener("click", () => {
      closeCenterNewTabMenu();
      this.startNewConversation(undefined, "kiro");
    });
    geminiMenuItem.addEventListener("click", () => {
      closeCenterNewTabMenu();
      this.startNewConversation(undefined, "gemini");
    });

    // Hover handlers for agent definition submenu
    for (const { kind, el: item } of menuItems) {
      item.addEventListener("mouseenter", () => showSubmenu(item, kind));
      item.addEventListener("mouseleave", () => hideSubmenu());
    }

    const handleMenuOutsidePointer = (event: PointerEvent): void => {
      const target = event.target as Node | null;
      if (
        !centerNewTabSplit.contains(target) &&
        !centerNewTabMenu.contains(target) &&
        !centerNewTabSubmenu.contains(target)
      ) {
        closeCenterNewTabMenu();
      }
    };
    document.addEventListener("pointerdown", handleMenuOutsidePointer);
    this.uiCleanupFns.push(() =>
      document.removeEventListener("pointerdown", handleMenuOutsidePointer),
    );

    const handleMenuEscape = (event: KeyboardEvent): void => {
      if (event.key === "Escape") closeCenterNewTabMenu();
    };
    window.addEventListener("keydown", handleMenuEscape);
    this.uiCleanupFns.push(() =>
      window.removeEventListener("keydown", handleMenuEscape),
    );

    const handleMenuReposition = (): void => {
      if (!centerNewTabMenu.hidden) positionCenterNewTabMenu();
    };
    window.addEventListener("resize", handleMenuReposition);
    window.addEventListener("scroll", handleMenuReposition, true);
    this.uiCleanupFns.push(() =>
      window.removeEventListener("resize", handleMenuReposition),
    );
    this.uiCleanupFns.push(() =>
      window.removeEventListener("scroll", handleMenuReposition, true),
    );
    this.uiCleanupFns.push(() => {
      closeCenterNewTabMenu();
      centerNewTabMenu.remove();
      centerNewTabSubmenu.remove();
    });

    this.centerNewTabBtn = centerNewTabBtn;
    this.centerNewTabMenuBtn = centerNewTabMenuBtn;
    this.centerNewTabMenu = centerNewTabMenu;
    this.centerNewTabMenuItems = {
      tycode: tycodeMenuItem,
      codex: codexMenuItem,
      claude: claudeMenuItem,
      kiro: kiroMenuItem,
      gemini: geminiMenuItem,
    };

    this.chatPanel.notificationManager = config.notifications;
    this.chatPanel.onNewChat = () => this.tabManager.onNewTab?.();
    this.chatPanel.onUserMessageSent = (conversationId, text) => {
      this.handleUserMessageForAutoTitle(conversationId, text);
    };
    this.chatPanel.onSlashCommand = (command) => {
      const workflow = this.workflowStore.getByTrigger(command);
      if (!workflow) return false;
      this.workflowsPanel.runWorkflow(workflow);
      this.layout.showWidget("workflows");
      return true;
    };

    this.eventRouter = new EventRouter({
      chatPanel: this.chatPanel,
      tabManager: this.tabManager,
      gitPanel: this.gitPanel,
      sessionsPanel: this.sessionsPanel,
      agentsPanel: this.agentsPanel,
      notifications: this.notifications,
      diffPanel: this.diffPanel,
    });

    this.wireTabCallbacks();
    this.wireSessionCallbacks();
    this.wireComponentCallbacks();
    this.wireTabDockDrag();
    this.updateNewConversationAvailability(
      this.newConversationEnabled,
      this.newConversationDisabledReason,
    );

    if (this.mode === "orchestrator") {
      this.showEmptyState();
    } else {
      this._constructorWork.push(
        this.gitPanel.discoverRepos(config.workspacePath).catch((err) => {
          console.error("Failed to discover git repos:", err);
        }),
      );
      this._constructorWork.push(
        this.fileExplorer.setRootPath(config.workspacePath),
      );
      this.chatPanel.showWelcome();
    }
    this._constructorWork.push(
      this.workflowStore.load().catch((err) => {
        console.error("Failed to load workflows:", err);
      }),
    );
  }

  show(): void {
    this.root.style.display = "";
    const work: Promise<unknown>[] = [];
    if (this.mode !== "orchestrator") {
      work.push(this.startFileWatching());
      work.push(this.refreshOpenFileTabs());
    }
    void this.requestSessionsList(false);
    if (this.mode === "orchestrator" && !this.tabManager.hasTabs()) {
      this.layout.setHomeMode(true);
    }
    this._readyPromise = Promise.all([...this._constructorWork, ...work]).then(
      () => {},
    );
  }

  whenReady(): Promise<void> {
    return this._readyPromise ?? Promise.resolve();
  }
  hide(): void {
    if (this.mode !== "orchestrator") {
      this.stopFileWatching();
    }
    this.root.style.display = "none";
  }
  destroy(): void {
    if (this.mode !== "orchestrator") {
      this.stopFileWatching();
    }
    this.workflowBuilder.destroy();
    this.terminalService.destroy();
    for (const cleanup of this.uiCleanupFns) {
      cleanup();
    }
    this.uiCleanupFns = [];
    for (const adminId of Object.values(this.adminIds)) {
      if (typeof adminId !== "number") continue;
      void closeAdminSubprocess(adminId).catch((err) =>
        console.error("Failed to close admin subprocess on destroy:", err),
      );
    }
    this.adminIds = {};
    this.adminBackendById.clear();
    this.sessionsByBackend.clear();
    this.rejectAllSessionsListWaiters("Workspace view disposed");
    this.conversationTydeSessionMap.clear();
    this.titleGenerationRequested.clear();
    this.sessionsRefreshRequestedForConversation.clear();
    for (const conversationId of this.conversationIds) {
      void closeConversation(conversationId).catch((err) =>
        console.error("Failed to close conversation on destroy:", err),
      );
      this.eventRouter.unregisterFeedbackAgent(conversationId);
    }
    this.conversationIds.clear();
    this.agentsPanel.clear();
    this.emitConversationIdsChanged();
  }

  getActiveConversationId(): number | null {
    const active = this.tabManager.getActiveTab();
    if (active?.kind === "chat") return active.conversationId;
    const fallback = this.tabManager.getPreferredChatTab();
    if (!fallback || fallback.kind !== "chat") return null;
    return fallback.conversationId;
  }

  getAdminId(): number | null {
    return this.adminIds.tycode ?? null;
  }
  ownsAdminId(adminId: number): boolean {
    return this.adminBackendById.has(adminId);
  }
  getTabManager(): TabManager {
    return this.tabManager;
  }
  getChatPanel(): ChatPanel {
    return this.chatPanel;
  }
  getLayout(): Layout {
    return this.layout;
  }
  getDiffPanel(): DiffPanel {
    return this.diffPanel;
  }
  getGitPanel(): GitPanel {
    return this.gitPanel;
  }
  getFileExplorer(): FileExplorer {
    return this.fileExplorer;
  }
  getSessionsPanel(): SessionsPanel {
    return this.sessionsPanel;
  }
  getAgentsPanel(): AgentsPanel {
    return this.agentsPanel;
  }
  getWorkflowStore(): WorkflowStore {
    return this.workflowStore;
  }
  getWorkflowEngine(): WorkflowEngine {
    return this.workflowEngine;
  }
  getWorkflowsPanel(): WorkflowsPanel {
    return this.workflowsPanel;
  }
  ownsConversation(conversationId: number): boolean {
    return this.conversationIds.has(conversationId);
  }
  getConversationIds(): number[] {
    return Array.from(this.conversationIds);
  }

  getHomeViewContainer(): HTMLElement | null {
    return this.homeViewContainer;
  }

  isBridgeMode(): boolean {
    return this.mode === "orchestrator";
  }

  updateNewConversationAvailability(
    enabled: boolean,
    reason?: string | null,
  ): void {
    this.newConversationEnabled = enabled;
    if (typeof reason === "string" && reason.trim().length > 0) {
      this.newConversationDisabledReason = reason;
    }
    const tooltip = enabled
      ? this.agentDefinitionId != null
        ? `New ${this.definitionLabel} chat (${formatShortcut("Ctrl+N")})`
        : `New tab (${formatShortcut("Ctrl+N")})`
      : this.newConversationDisabledReason;
    if (this.centerNewTabBtn) {
      this.centerNewTabBtn.disabled = !enabled;
      this.centerNewTabBtn.title = tooltip;
    }
    if (this.centerNewTabMenuBtn) {
      this.centerNewTabMenuBtn.disabled = !enabled;
      this.centerNewTabMenuBtn.title = enabled
        ? this.agentDefinitionId != null
          ? `New ${this.definitionLabel} chat options`
          : "New chat options"
        : this.newConversationDisabledReason;
    }
    for (const item of Object.values(this.centerNewTabMenuItems)) {
      if (!item) continue;
      item.disabled = !enabled;
      if (!enabled) {
        item.title = this.newConversationDisabledReason;
      } else {
        item.removeAttribute("title");
      }
    }
    this.sessionsPanel.setNewSessionAvailability(
      enabled,
      this.newConversationDisabledReason,
    );
  }

  setRoots(roots: string[]): void {
    this.roots = roots;
    this.workflowEngine.setWorkspaceRoots(this.getEffectiveRoots());
  }

  private getEffectiveRoots(): string[] {
    if (this.roots.length === 0) return [this.workspacePath];
    return this.roots.map((r) => `${this.workspacePath}/${r}`);
  }

  async refreshHostSettings(): Promise<void> {
    const p = this._doRefreshHostSettings();
    this.hostSettingsReady = p;
    return p;
  }

  private async _doRefreshHostSettings(): Promise<void> {
    try {
      const hosts = await listHosts();
      const host = this.resolveHostForWorkspace(hosts);
      if (!host) return;
      this.resolvedHostId = host.id;

      const nextEnabled = host.enabled_backends
        .map((kind) => normalizeBackendKind(kind))
        .filter((kind, index, list) => list.indexOf(kind) === index);
      const nextDefault = normalizeBackendKind(host.default_backend);

      const enabledChanged =
        !this.hostEnabledBackends ||
        this.hostEnabledBackends.length !== nextEnabled.length ||
        this.hostEnabledBackends.some((kind, idx) => kind !== nextEnabled[idx]);
      const defaultChanged = this.hostDefaultBackend !== nextDefault;

      this.hostEnabledBackends = nextEnabled;
      this.hostDefaultBackend = nextDefault;

      if (enabledChanged || defaultChanged) {
        this.refreshNewChatMenu();
      }
    } catch (err) {
      console.error("Failed to load host settings for workspace:", err);
    }
  }

  private resolveHostForWorkspace(hosts: Host[]): Host | null {
    const remote = parseRemoteWorkspaceUri(this.workspacePath);
    if (remote) {
      return hosts.find((host) => host.hostname === remote.host) ?? null;
    }
    return hosts.find((host) => host.is_local) ?? null;
  }

  private getWorkspaceEnabledBackends(): BackendKind[] {
    if (!this.hostEnabledBackends) return [];
    return [...this.hostEnabledBackends];
  }

  private getWorkspaceDefaultBackend(): BackendKind | null {
    return this.hostDefaultBackend;
  }

  refreshNewChatMenu(): void {
    const menu = this.centerNewTabMenu;
    if (!menu) return;
    while (menu.firstChild) menu.removeChild(menu.firstChild);
    const enabledSet = new Set(this.getWorkspaceEnabledBackends());
    if (enabledSet.size === 0) {
      const hint = document.createElement("div");
      hint.className = "center-tab-new-menu-empty";
      hint.textContent =
        "No backends enabled. Enable at least one in Settings → Backends.";
      menu.appendChild(hint);
    }
    const order: (keyof typeof this.centerNewTabMenuItems)[] = [
      "tycode",
      "codex",
      "claude",
      "kiro",
      "gemini",
    ];
    for (const kind of order) {
      const item = this.centerNewTabMenuItems[kind];
      if (item && enabledSet.has(kind)) {
        menu.appendChild(item);
      }
    }
  }

  showEmptyState(): void {
    this.chatPanel.clear();
    this.diffPanel.clear();
    if (this.mode === "orchestrator") {
      this.layout.setHomeMode(true);
      return;
    }
    this.layout.switchTab("chat");
    this.chatPanel.showWelcome();
  }

  focusConversation(conversationId: number, title?: string): boolean {
    const existingTab = this.tabManager.getTabByConversationId(conversationId);
    if (existingTab) {
      if (this.tabManager.getActiveTab()?.id !== existingTab.id) {
        this.tabManager.switchTo(existingTab.id);
      } else {
        this.layout.setHomeMode(false);
        this.layout.switchTab("chat");
        this.chatPanel.switchToConversation(conversationId);
      }
      return true;
    }

    if (this.layout.activateDockedConversation(conversationId)) {
      return true;
    }

    if (!this.ownsConversation(conversationId)) return false;
    const tab = this.tabManager.createChatTab(conversationId, title);
    this.tabManager.switchTo(tab.id);
    return true;
  }

  syncRuntimeAgent(agent: RuntimeAgent): void {
    const backendKind = normalizeBackendKind(agent.backend_kind);
    const currentBackendKind = this.conversationBackendKindMap.get(
      agent.conversation_id,
    );
    if (currentBackendKind !== backendKind) {
      this.chatPanel.setConversationBackendKind(
        agent.conversation_id,
        backendKind,
      );
      this.conversationBackendKindMap.set(agent.conversation_id, backendKind);
    }
    this.registerConversation(this.runtimeAgentToPanelInfo(agent));
  }

  syncRuntimeAgents(agents: RuntimeAgent[]): void {
    const nextConversationIds = new Set<number>();
    for (const agent of agents) {
      nextConversationIds.add(agent.conversation_id);
      this.syncRuntimeAgent(agent);
    }

    for (const existing of this.agentsPanel.getAgents()) {
      if (!existing.agentId) continue;
      if (nextConversationIds.has(existing.conversationId)) continue;
      this.agentsPanel.removeAgent(existing.conversationId);
    }
  }

  syncRuntimeAgentPreviews(agents: RuntimeAgent[]): void {
    const nextConversationIds = new Set<number>();
    for (const agent of agents) {
      nextConversationIds.add(agent.conversation_id);
      // Preview cards are not owned by this view's EventRouter, so always
      // use the backend summary directly instead of runtimeAgentToPanelInfo
      // which preserves stale EventRouter state.
      let name = agent.name.trim() || `Agent ${agent.agent_id}`;
      const existing = this.agentsPanel.getAgentByConversationId(
        agent.conversation_id,
      );
      if (existing?.name && (name === "Bridge" || name === "Conversation")) {
        name = existing.name;
        void renameAgent(agent.agent_id, name);
      }
      this.agentsPanel.upsertAgent({
        agentId: agent.agent_id,
        conversationId: agent.conversation_id,
        name,
        agentType: agent.agent_type,
        createdAt: agent.created_at_ms,
        projectId: this.projectId,
        parentAgentId: agent.parent_agent_id,
        summary: agent.is_running
          ? agent.summary.trim() || "Running..."
          : (agent.last_error ?? (agent.summary.trim() || "Completed")),
        isTyping: agent.is_running,
        hasError: agent.last_error != null,
      });
    }

    for (const existing of this.agentsPanel.getAgents()) {
      if (!existing.agentId) continue;
      if (nextConversationIds.has(existing.conversationId)) continue;
      this.agentsPanel.removeAgent(existing.conversationId);
    }
  }

  focusFindInActiveFileViewer(): boolean {
    const active = this.tabManager.getActiveTab();
    if (!active || active.kind !== "file" || active.fileView !== "file")
      return false;
    this.layout.switchTab("diff");
    return this.diffPanel.focusFind();
  }

  focusGoToLineInActiveFileViewer(): boolean {
    const active = this.tabManager.getActiveTab();
    if (!active || active.kind !== "file" || active.fileView !== "file")
      return false;
    this.layout.switchTab("diff");
    return this.diffPanel.focusGoToLine();
  }

  private resolveLinkedFilePath(rawPath: string): string {
    const path = rawPath.trim();
    if (!path) return path;
    if (path.startsWith("/")) return path;
    if (path.startsWith("ssh://")) return path;
    if (/^[A-Za-z]:[\\/]/.test(path)) return path;

    const relative = path.replace(/^\.\//, "");
    const base = this.workspacePath.endsWith("/")
      ? this.workspacePath.slice(0, -1)
      : this.workspacePath;
    return `${base}/${relative}`;
  }

  private async openFileFromLinkedMessage(
    filePath: string,
    oneBasedLine?: number,
  ): Promise<void> {
    const resolvedPath = this.resolveLinkedFilePath(filePath);
    const result = await readFileContent(resolvedPath);
    this.openFileViewerTab(result.content, result.path, oneBasedLine);
  }

  handleChatEvent(payload: ChatEventPayload): void {
    this.eventRouter.handleChatEvent(payload);

    if (
      payload.event.kind === "StreamStart" &&
      this.conversationIds.has(payload.conversation_id)
    ) {
      this.requestSessionsRefreshForStartedConversation(
        payload.conversation_id,
      );
    }

    if (
      payload.event.kind === "SessionStarted" &&
      this.conversationBackendKindMap.has(payload.conversation_id)
    ) {
      const sessionId = payload.event.data.session_id;
      if (sessionId) {
        const backendKind = this.conversationBackendKindMap.get(
          payload.conversation_id,
        )!;
        this.conversationSessionMap.set(payload.conversation_id, {
          sessionId,
          backendKind,
        });
      }
    }
  }

  handleAdminEvent(payload: AdminEventPayload): void {
    const event = payload.event;
    if (event.kind === "SubprocessExit") {
      const backendKind = this.adminBackendById.get(payload.admin_id);
      if (backendKind) this.invalidateAdminSubprocess(backendKind);
      return;
    }
    if (event.kind === "SessionsList") {
      this.eventRouter.clearSessionsLoadingTimeout();
      const backendKind =
        this.adminBackendById.get(payload.admin_id) ?? "tycode";
      this.resolveSessionsListWaiters(backendKind);
      // Store backend sessions for the external sessions toggle
      const sessions = event.data.sessions.map((session) => ({
        ...session,
        backend_kind: session.backend_kind ?? backendKind,
      }));
      this.sessionsByBackend.set(backendKind, sessions);
      // Refresh the session panel from the store
      void this.sessionsPanel.refresh();
    }
  }

  private async applyDefaultSpawnProfile(
    conversationId: number,
    backendKind: BackendKind,
  ): Promise<void> {
    if (backendKind !== "tycode") return;
    const profile = getDefaultSpawnProfile();
    if (!profile) return;
    try {
      await switchProfile(conversationId, profile);
    } catch (err) {
      console.warn(
        `Failed to apply default spawn profile "${profile}" to conversation ${conversationId}:`,
        err,
      );
    }
  }

  private handleUserMessageForAutoTitle(
    conversationId: number,
    text: string,
  ): void {
    const trimmed = text.trim();
    if (!trimmed) return;
    if (this.titleGenerationRequested.has(conversationId)) return;
    if (!this.tabManager.canAutoRenameChatTab(conversationId)) return;
    this.titleGenerationRequested.add(conversationId);
    void this.generateAutoTitle(conversationId, trimmed);
  }

  private async generateAutoTitle(
    conversationId: number,
    firstMessage: string,
  ): Promise<void> {
    const backendKind =
      this.conversationBackendKindMap.get(conversationId) ??
      this.resolveConversationBackend();
    const prompt = this.buildTitlePrompt(firstMessage);
    const titleRoots = this.resolveTitleWorkspaceRoots();
    if (titleRoots.length === 0) {
      this.titleGenerationRequested.delete(conversationId);
      return;
    }
    let titleAgentId: string | null = null;

    try {
      const spawned = await spawnAgent(
        titleRoots,
        prompt,
        backendKind,
        undefined,
        `${INTERNAL_TITLE_AGENT_PREFIX}${conversationId}`,
        true,
      );
      titleAgentId = spawned.agent_id;
      await waitForAgent(titleAgentId);
      const result = await collectAgentResult(titleAgentId);
      const rawTitle =
        result.final_message ??
        result.agent.last_message ??
        result.agent.summary;
      const normalizedTitle = this.normalizeGeneratedTitle(rawTitle ?? "");
      if (!normalizedTitle) return;

      const renamed = this.tabManager.autoRenameChatTab(
        conversationId,
        normalizedTitle,
        "system",
      );
      if (!renamed) return;

      this.agentsPanel.updateAgent(conversationId, { name: normalizedTitle });
      const agentInfo =
        this.agentsPanel.getAgentByConversationId(conversationId);
      if (agentInfo?.agentId != null) {
        void renameAgent(agentInfo.agentId, normalizedTitle);
      }
      const tydeSessionId = this.conversationTydeSessionMap.get(conversationId);
      if (tydeSessionId) {
        void setSessionAlias(tydeSessionId, normalizedTitle);
      }
    } catch (err) {
      console.warn(
        `Auto-title generation failed for conversation ${conversationId}:`,
        err,
      );
    } finally {
      if (titleAgentId !== null) {
        void terminateAgent(titleAgentId).catch((err) =>
          console.error("Failed to terminate title agent:", err),
        );
      }
    }
  }

  private buildTitlePrompt(firstMessage: string): string {
    const clipped = firstMessage.replace(/\s+/g, " ").trim().slice(0, 600);
    return [
      "Generate a concise title for this chat session.",
      "Constraints:",
      "- 2 to 4 words",
      "- plain text only",
      "- no quotes, no punctuation at the ends, no markdown",
      "- summarize the user intent",
      "",
      `User message: ${clipped}`,
      "",
      "Return only the title text.",
    ].join("\n");
  }

  private normalizeGeneratedTitle(raw: string): string | null {
    const compact = raw.replace(/\s+/g, " ").trim();
    if (!compact) return null;

    const noPrefix = compact.replace(/^(?:title|session title)\s*[:-]\s*/i, "");
    const dequoted = noPrefix.replace(/^["'`]+|["'`]+$/g, "").trim();
    if (!dequoted) return null;

    const words =
      dequoted.match(/[A-Za-z0-9]+(?:['/&.+-][A-Za-z0-9]+)*/g) ?? [];
    if (words.length === 0) return null;

    const normalized = words.slice(0, 4).join(" ");
    return normalized.length > 60 ? normalized.slice(0, 60).trim() : normalized;
  }

  private resolveTitleWorkspaceRoots(): string[] {
    if (this.mode !== "orchestrator") {
      return this.workspacePath.trim() ? [this.workspacePath] : [];
    }
    const projects = this.getBridgeProjects();
    if (projects.length === 0) return [];
    return [projects[0].workspacePath];
  }

  refreshDefinitionLabel(): void {
    this.definitionLabel = this.resolveDefinitionLabel();
  }

  private resolveDefinitionLabel(): string {
    if (!this.agentDefinitionId || !this.agentDefinitionStore) return "Agent";
    const def = this.agentDefinitionStore.getById(this.agentDefinitionId);
    return def?.name?.trim() || "Agent";
  }

  private async resolveBootstrapPrompt(
    definitionId?: string,
  ): Promise<string | null> {
    if (!definitionId || !this.agentDefinitionStore) return null;
    const def = this.agentDefinitionStore.getById(definitionId);
    if (!def) return null;

    if (def.bootstrap_prompt) return def.bootstrap_prompt;

    if (def.include_agent_control && this.mode === "orchestrator") {
      return this.buildOrchestratorBootstrap(def);
    }

    return null;
  }

  private buildOrchestratorBootstrap(def: AgentDefinitionEntry): string {
    const projects = this.getBridgeProjects();
    const projectLines =
      projects.length === 0
        ? ["- No projects are currently open in Tyde."]
        : projects.map((p, i) => {
            const rootsInfo =
              p.roots.length > 0 ? ` (sub-roots: ${p.roots.join(", ")})` : "";
            return `- ${i + 1}. ${p.name} :: ${p.workspacePath}${rootsInfo}`;
          });
    return [
      `[Tyde ${def.name} Charter]`,
      `You are the ${def.name}, a control agent operating inside Tyde.`,
      "Your role is to coordinate work between the human and other Tyde agents.",
      "Rules:",
      "- Do not directly perform implementation work yourself.",
      "- Delegate concrete execution to other agents using the Tyde agent control MCP tools.",
      "- Choose the right workspace and agent for each task, monitor progress, wait for results, and report back to the human.",
      "- Ask clarifying questions when the objective or success criteria are unclear.",
      "- Keep your own messages focused on coordination, delegation, status tracking, and synthesis.",
      "",
      "Projects currently open in Tyde:",
      ...projectLines,
      "",
      "Respond with a brief acknowledgment that you understand this operating mode, then wait for the human.",
    ].join("\n");
  }

  private resolveWorkspaceRootsForBackend(backendKind: BackendKind): string[] {
    if (this.mode !== "bridge" && this.mode !== "orchestrator") {
      if (!this.workspacePath.trim()) return [];
      if (backendKind === "tycode") {
        return this.getEffectiveRoots();
      }
      return [this.workspacePath];
    }
    if (backendKind === "tycode") {
      return [];
    }
    return this.getBridgeProjects()
      .map((project) => project.workspacePath)
      .filter(
        (workspacePath) =>
          workspacePath.trim().length > 0 &&
          (backendKind === "kiro" ||
            backendKind === "gemini" ||
            !workspacePath.startsWith("ssh://")),
      );
  }

  private resolveTitleForNewConversation(
    tabLabel?: string,
  ): string | undefined {
    const trimmed = tabLabel?.trim();
    if (trimmed) return trimmed;
    if (this.mode !== "orchestrator") return undefined;
    const nextIndex =
      this.tabManager.getTabs().filter((tab) => tab.kind === "chat").length + 1;
    return `${this.definitionLabel} ${nextIndex}`;
  }

  private ensureConversationCreationAllowed(): void {
    if (this.newConversationEnabled) return;
    throw new Error(this.newConversationDisabledReason);
  }

  async createNewConversationTab(
    tabLabel?: string,
    backendOverride?: BackendKind,
    options?: { bootstrap?: boolean; agentDefinitionId?: string },
  ): Promise<number> {
    this.ensureConversationCreationAllowed();
    await this.hostSettingsReady;
    const backendKind = this.resolveConversationBackend(backendOverride);
    const effectiveDefinitionId =
      options?.agentDefinitionId ?? this.agentDefinitionId;

    // Show tab with loading state BEFORE blocking subprocess spawn
    const tabTitle = this.resolveTitleForNewConversation(tabLabel);
    const tab = this.tabManager.createChatTab(null, tabTitle);
    this.tabManager.switchTo(tab.id);
    this.layout.setHomeMode(false);
    this.layout.switchTab("chat");
    this.chatPanel.showSpawnLoading();

    let id: number;
    let tydeSessionId: string | undefined;
    try {
      await this.ensureAdminSubprocess(backendKind);
      const conversationRoots =
        this.resolveWorkspaceRootsForBackend(backendKind);
      const result = await createConversation(
        conversationRoots,
        backendKind,
        undefined,
        effectiveDefinitionId,
      );
      id = result.conversation_id;
      tydeSessionId = result.session_id;
    } catch (err) {
      this.tabManager.closeTab(tab.id);
      this.chatPanel.showSpawnError(
        `Failed to start agent: ${err instanceof Error ? err.message : String(err)}`,
      );
      throw err;
    }

    // If the user closed the tab while we were awaiting, clean up and bail out.
    const tabStillExists = this.tabManager
      .getTabs()
      .some((t) => t.id === tab.id);
    if (!tabStillExists) {
      closeConversation(id).catch((err) =>
        console.error("Failed to close orphaned conversation:", err),
      );
      return id;
    }

    // Bind the real conversation ID to the tab and view
    tab.conversationId = id;
    if (tydeSessionId) {
      this.conversationTydeSessionMap.set(id, tydeSessionId);
    }
    this.chatPanel.setConversationBackendKind(id, backendKind);
    this.conversationBackendKindMap.set(id, backendKind);
    this.registerConversation({
      conversationId: id,
      name: tab.title,
      summary: "Ready",
      isTyping: false,
      createdAt: Date.now(),
      projectId: this.projectId,
    });
    this.chatPanel.switchToConversation(id);
    await this.applyDefaultSpawnProfile(id, backendKind);
    if (effectiveDefinitionId && options?.bootstrap !== false) {
      const bootstrap = await this.resolveBootstrapPrompt(
        effectiveDefinitionId,
      );
      if (bootstrap) {
        await sendMessage(id, bootstrap);
      }
    }
    return id;
  }

  async ensureAdminSubprocess(
    backendKind: BackendKind = "tycode",
  ): Promise<number> {
    const existing = this.adminIds[backendKind];
    if (typeof existing === "number") return existing;
    const id = await createAdminSubprocess(
      this.resolveWorkspaceRootsForBackend(backendKind),
      backendKind,
    );
    this.adminIds[backendKind] = id;
    this.adminBackendById.set(id, backendKind);
    return id;
  }

  private invalidateAdminSubprocess(backendKind: BackendKind): void {
    this.rejectSessionsListWaiters(
      backendKind,
      new Error(`${backendKind} admin subprocess is unavailable`),
    );
    const id = this.adminIds[backendKind];
    if (typeof id === "number") {
      this.adminBackendById.delete(id);
    }
    delete this.adminIds[backendKind];
  }

  private supportedSessionBackends(): BackendKind[] {
    const enabled = new Set(this.getWorkspaceEnabledBackends());
    const backends: BackendKind[] = [];
    if (enabled.has("tycode")) backends.push("tycode");
    if (
      enabled.has("codex") &&
      this.resolveWorkspaceRootsForBackend("codex").length > 0
    ) {
      backends.push("codex");
    }
    if (
      enabled.has("claude") &&
      this.resolveWorkspaceRootsForBackend("claude").length > 0
    ) {
      backends.push("claude");
    }
    if (
      enabled.has("kiro") &&
      this.resolveWorkspaceRootsForBackend("kiro").length > 0
    ) {
      backends.push("kiro");
    }
    if (
      enabled.has("gemini") &&
      this.resolveWorkspaceRootsForBackend("gemini").length > 0
    ) {
      backends.push("gemini");
    }
    return backends;
  }

  private createSessionsListWaiter(backendKind: BackendKind): {
    promise: Promise<void>;
    cancel: () => void;
  } {
    const waiters =
      this.sessionsListWaitersByBackend.get(backendKind) ??
      new Set<SessionsListWaiter>();
    this.sessionsListWaitersByBackend.set(backendKind, waiters);

    let waiter: SessionsListWaiter | null = null;
    const promise = new Promise<void>((resolve, reject) => {
      const timeoutId = setTimeout(() => {
        if (waiter) this.removeSessionsListWaiter(backendKind, waiter);
        reject(new Error(`${backendKind} sessions list refresh timed out`));
      }, SESSIONS_LIST_WAIT_TIMEOUT_MS);
      const newWaiter: SessionsListWaiter = { resolve, reject, timeoutId };
      waiter = newWaiter;
      waiters.add(newWaiter);
    });

    const cancel = (): void => {
      if (!waiter) return;
      clearTimeout(waiter.timeoutId);
      this.removeSessionsListWaiter(backendKind, waiter);
      waiter = null;
    };

    return { promise, cancel };
  }

  private removeSessionsListWaiter(
    backendKind: BackendKind,
    waiter: SessionsListWaiter,
  ): void {
    const waiters = this.sessionsListWaitersByBackend.get(backendKind);
    if (!waiters) return;
    waiters.delete(waiter);
    if (waiters.size === 0) {
      this.sessionsListWaitersByBackend.delete(backendKind);
    }
  }

  private resolveSessionsListWaiters(backendKind: BackendKind): void {
    const waiters = this.sessionsListWaitersByBackend.get(backendKind);
    if (!waiters || waiters.size === 0) return;
    this.sessionsListWaitersByBackend.delete(backendKind);
    for (const waiter of waiters) {
      clearTimeout(waiter.timeoutId);
      waiter.resolve();
    }
  }

  private rejectSessionsListWaiters(
    backendKind: BackendKind,
    reason: Error,
  ): void {
    const waiters = this.sessionsListWaitersByBackend.get(backendKind);
    if (!waiters || waiters.size === 0) return;
    this.sessionsListWaitersByBackend.delete(backendKind);
    for (const waiter of waiters) {
      clearTimeout(waiter.timeoutId);
      waiter.reject(reason);
    }
  }

  private rejectAllSessionsListWaiters(reason: string): void {
    for (const backendKind of this.supportedSessionBackends()) {
      this.rejectSessionsListWaiters(backendKind, new Error(reason));
    }
    this.sessionsListWaitersByBackend.clear();
  }

  private async refreshSessionsForBackend(
    backendKind: BackendKind,
  ): Promise<void> {
    let lastError: unknown = null;
    for (let attempt = 0; attempt < 2; attempt++) {
      let adminId: number;
      adminId = await this.ensureAdminSubprocess(backendKind);

      const listWaiter = this.createSessionsListWaiter(backendKind);
      try {
        await adminListSessions(adminId);
        await listWaiter.promise;
        return;
      } catch (err) {
        listWaiter.cancel();
        lastError = err;
        this.invalidateAdminSubprocess(backendKind);
      }
    }
    throw lastError instanceof Error
      ? lastError
      : new Error(String(lastError ?? "Unknown error"));
  }

  openFileViewerTab(
    content: string,
    path: string,
    oneBasedLine?: number,
  ): string {
    const existing = this.tabManager.getTabByFilePath(path, "file");
    if (existing) {
      this.diffPanel.showFileContent(content, path, existing.id);
      this.tabManager.switchTo(existing.id);
      if (oneBasedLine !== undefined) {
        this.diffPanel.revealFileLine(existing.id, oneBasedLine);
      }
      return existing.id;
    }
    const tab = this.tabManager.createFileTab(path, "file", basename(path));
    this.diffPanel.showFileContent(content, path, tab.id);
    this.tabManager.switchTo(tab.id);
    if (oneBasedLine !== undefined) {
      this.diffPanel.revealFileLine(tab.id, oneBasedLine);
    }
    return tab.id;
  }

  openDiffTab(diff: string, path: string): void {
    const existing = this.tabManager.getTabByFilePath(path, "diff");
    if (existing) {
      this.diffPanel.showUnifiedDiff(diff, path, existing.id);
      this.tabManager.switchTo(existing.id);
      return;
    }
    const tab = this.tabManager.createFileTab(path, "diff", basename(path));
    this.diffPanel.showUnifiedDiff(diff, path, tab.id);
    this.tabManager.switchTo(tab.id);
  }

  openBeforeAfterDiffTab(before: string, after: string, path: string): void {
    const existing = this.tabManager.getTabByFilePath(path, "diff");
    if (existing) {
      this.diffPanel.showBeforeAfterDiff(before, after, path, existing.id);
      this.tabManager.switchTo(existing.id);
      return;
    }
    const tab = this.tabManager.createFileTab(path, "diff", basename(path));
    this.diffPanel.showBeforeAfterDiff(before, after, path, tab.id);
    this.tabManager.switchTo(tab.id);
  }

  async focusChatTabOrCreate(): Promise<void> {
    const chatTab = this.tabManager.getPreferredChatTab();
    if (chatTab) {
      this.tabManager.switchTo(chatTab.id);
      this.chatPanel.focusInput();
      return;
    }
    try {
      await this.createNewConversationTab();
      this.chatPanel.focusInput();
    } catch (err) {
      console.error("Failed to create conversation:", err);
      this.notifications.error(
        err instanceof Error ? err.message : "Failed to create conversation",
      );
    }
  }

  async requestSessionsList(showPanel: boolean = true): Promise<void> {
    if (showPanel) {
      this.layout.showWidget("sessions");
    }
    if (showPanel) {
      this.eventRouter.beginSessionsLoading();
    }

    try {
      const records = await listSessionRecords();
      this.eventRouter.clearSessionsLoadingTimeout();
      this.sessionsPanel.updateFromRecords(records);
    } catch (err) {
      if (showPanel) {
        this.eventRouter.clearSessionsLoadingTimeout();
        this.sessionsPanel.showError(
          `Failed to load sessions: ${err instanceof Error ? err.message : String(err)}`,
        );
      } else {
        console.warn("Background session refresh failed:", err);
      }
    }
  }

  private async createDockedTerminal(
    zone: "left" | "right" | "bottom" = "bottom",
  ): Promise<void> {
    if (this.mode === "orchestrator" && !this.workspacePath.trim()) {
      this.notifications.warning(
        `${this.definitionLabel} chats do not expose a workspace terminal.`,
      );
      return;
    }
    try {
      const session = await this.terminalService.createSession();
      this.layout.dockTerminalView(
        session.id,
        zone,
        session.viewEl,
        session.label,
      );
    } catch (err) {
      this.notifications.error(`Failed to start terminal: ${String(err)}`);
    }
  }

  private async closeDockedTerminal(terminalId: number): Promise<void> {
    this.layout.removeDockedTerminalView(terminalId);
    await this.terminalService.closeSession(terminalId);
  }

  private wireTabCallbacks(): void {
    this.tabManager.onBeforeTabSwitch = null;

    this.tabManager.onTabSwitch = (tab: TabState) => {
      const start = perfNow();
      let layoutMs = 0;
      let chatSwitchMs = 0;
      let diffActivateMs = 0;
      let fileRefreshKickMs = 0;
      let watchSyncKickMs = 0;

      this.layout.setHomeMode(false);

      if (tab.kind === "chat") {
        const layoutStart = perfNow();
        this.layout.switchTab("chat");
        layoutMs = perfNow() - layoutStart;
        if (tab.conversationId !== null) {
          const chatSwitchStart = perfNow();
          this.chatPanel.switchToConversation(tab.conversationId);
          chatSwitchMs = perfNow() - chatSwitchStart;
        }
      } else if (tab.kind === "file") {
        const layoutStart = perfNow();
        this.layout.switchTab("diff");
        layoutMs = perfNow() - layoutStart;
        const activateStart = perfNow();
        this.diffPanel.activateTab(tab.id);
        diffActivateMs = perfNow() - activateStart;
        if (tab.fileView === "file" && tab.filePath) {
          const asyncStart = perfNow();
          const refreshPromise = this.refreshFileContentPath(tab.filePath);
          fileRefreshKickMs = perfNow() - asyncStart;
          void refreshPromise
            .then(() => {
              logTabPerf(
                "WorkspaceView.onTabSwitch file refresh",
                perfNow() - asyncStart,
                {
                  tabId: tab.id,
                  filePath: tab.filePath,
                },
              );
            })
            .catch((err) => {
              logTabPerf(
                "WorkspaceView.onTabSwitch file refresh error",
                perfNow() - asyncStart,
                {
                  tabId: tab.id,
                  filePath: tab.filePath,
                  error: String(err),
                },
              );
            });
        }
      }
      const watchSyncStart = perfNow();
      const watchSyncPromise = this.syncFileWatchSubscriptions();
      watchSyncKickMs = perfNow() - watchSyncStart;
      void watchSyncPromise
        .then(() => {
          logTabPerf(
            "WorkspaceView.onTabSwitch syncFileWatchSubscriptions",
            perfNow() - watchSyncStart,
            {
              tabId: tab.id,
              tabKind: tab.kind,
            },
          );
        })
        .catch((err) => {
          logTabPerf(
            "WorkspaceView.onTabSwitch syncFileWatchSubscriptions error",
            perfNow() - watchSyncStart,
            {
              tabId: tab.id,
              tabKind: tab.kind,
              error: String(err),
            },
          );
        });

      logTabPerf("WorkspaceView.onTabSwitch", perfNow() - start, {
        tabId: tab.id,
        tabKind: tab.kind,
        layoutMs,
        chatSwitchMs,
        diffActivateMs,
        fileRefreshKickMs,
        watchSyncKickMs,
      });
    };

    this.tabManager.onTabClose = (tab: TabState) => {
      if (tab.kind === "file") {
        this.diffPanel.closeTabById(tab.id);
      }
      if (tab.kind === "chat" && tab.conversationId !== null) {
        const conversationId = tab.conversationId;
        const agent = this.agentsPanel.getAgentByConversationId(conversationId);
        const shouldPreserve = agent?.agentId != null;
        if (!shouldPreserve) {
          this.closeConversationPermanently(conversationId);
        }
      }
      if (!this.tabManager.hasTabs()) {
        this.showEmptyState();
      }
      if (this.mode !== "orchestrator") {
        void this.syncFileWatchSubscriptions();
      }
    };

    this.tabManager.onTabRenamed = (tab: TabState) => {
      if (tab.kind !== "chat" || tab.conversationId === null) return;
      this.agentsPanel.updateAgent(tab.conversationId, { name: tab.title });
      // User rename: write to session store via renameSession
      const tydeSessionId = this.conversationTydeSessionMap.get(
        tab.conversationId,
      );
      if (tydeSessionId) {
        void renameSession(tydeSessionId, tab.title).then(() =>
          this.sessionsPanel.refresh(),
        );
      }
    };

    this.tabManager.onNewTab = () => {
      this.startNewConversation();
    };
  }

  private wireSessionCallbacks(): void {
    this.sessionsPanel.onResumeSession = async (sessionId, backendKind) => {
      let cid: number | null = null;
      try {
        const existingConversationId = this.findConversationBySession(
          sessionId,
          backendKind,
        );
        if (existingConversationId !== null) {
          this.focusConversation(existingConversationId);
          this.sessionsPanel.setActiveSession(sessionId, backendKind);
          this.sessionsPanel.setResuming(null);
          this.gitPanel.requestRefresh();
          return;
        }

        // Always resume into a fresh tab; use saved alias as tab label if available.
        const savedAlias = this.sessionsPanel.getResolvedAliasForBackendSession(
          sessionId,
          backendKind,
        );
        cid = await this.createNewConversationTab(savedAlias, backendKind, {
          bootstrap: false,
        });
        this.chatPanel.setHistoryLoading(cid, true);
        await resumeSession(cid, sessionId);
        this.chatPanel.setHistoryLoading(cid, false);
        this.conversationSessionMap.set(cid, { sessionId, backendKind });
        this.sessionsPanel.setActiveSession(sessionId, backendKind);
        this.sessionsPanel.setResuming(null);
        this.gitPanel.requestRefresh();
      } catch (err) {
        if (cid !== null) {
          this.chatPanel.setHistoryLoading(cid, false);
          this.disposeFailedResumeConversation(cid);
        }
        this.sessionsPanel.setResuming(null);
        this.notifications.error(`Failed to resume session: ${String(err)}`);
      }
    };

    this.sessionsPanel.onNewSession = async () => {
      try {
        await this.createNewConversationTab();
      } catch (err) {
        console.error("Failed to create conversation:", err);
        this.notifications.error(
          err instanceof Error ? err.message : "Failed to create conversation",
        );
      }
    };

    this.sessionsPanel.onRefresh = () => {
      void this.requestSessionsList();
    };

    this.sessionsPanel.onDeleteSession = async (
      sessionId,
      backendKind,
      tydeSessionId,
    ) => {
      try {
        if (tydeSessionId) {
          // Store-sourced session: Rust handles backend deletion + store cleanup,
          // including routing to remote TydeServer when appropriate.
          await deleteSessionRecord(tydeSessionId);
        } else {
          // External session (no store record): use admin subprocess to delete
          // from the backend directly.
          const adminId = await this.ensureAdminSubprocess(backendKind);
          await adminDeleteSession(adminId, sessionId);
        }
        // Close the tab if this session is currently open.
        for (const [cid, info] of this.conversationSessionMap) {
          if (
            info.sessionId === sessionId &&
            info.backendKind === backendKind
          ) {
            const tab = this.tabManager.getTabByConversationId(cid);
            if (tab) this.tabManager.closeTab(tab.id);
            this.closeConversationPermanently(cid, false);
            break;
          }
        }
        await this.sessionsPanel.refresh();
        this.notifications.success("Session deleted");
      } catch (err) {
        this.notifications.error(`Failed to delete session: ${String(err)}`);
      }
    };

    this.sessionsPanel.onRequestExternalSessions = async () => {
      const backends = this.supportedSessionBackends();
      await Promise.all(
        backends.map(async (backendKind) => {
          try {
            await this.refreshSessionsForBackend(backendKind);
          } catch (err) {
            console.warn(
              `Failed to fetch ${backendKind} sessions for external toggle:`,
              err,
            );
          }
        }),
      );
      const allExternal: import("@tyde/protocol").SessionMetadata[] = [];
      for (const backendKind of backends) {
        const sessions = this.sessionsByBackend.get(backendKind) ?? [];
        for (const session of sessions) {
          allExternal.push({
            ...session,
            backend_kind: session.backend_kind ?? backendKind,
          });
        }
      }
      this.sessionsPanel.updateExternalSessions(allExternal);
    };
  }

  private wireTabDockDrag(): void {
    this.tabManager.onExternalDragStart = () => {
      this.layout.beginTabDockDrag();
    };

    this.tabManager.onExternalDragMove = (clientX, clientY) => {
      this.layout.updateTabDockDrag(clientX, clientY);
    };

    this.tabManager.onExternalDragEnd = (tab, _clientX, _clientY) => {
      const zone = this.layout.endTabDockDrag();
      if (!zone || tab.kind !== "chat" || tab.conversationId === null) return;

      const conversationId = tab.conversationId;
      if (this.layout.hasDockedConversation(conversationId)) return;

      const viewEl = this.chatPanel.detachView(conversationId);
      if (!viewEl) return;

      this.layout.dockConversationView(conversationId, zone, viewEl, tab.title);
      this.tabManager.removeTab(tab.id);
      const activeTab = this.tabManager.getActiveTab();
      if (activeTab) {
        this.tabManager.onTabSwitch?.(activeTab);
      }
    };

    this.layout.onUndockConversation = (conversationId) => {
      const title = this.layout.getDockedConversationTitle(conversationId);
      const viewEl = this.layout.undockConversationView(conversationId);
      if (!viewEl) return;

      this.chatPanel.reattachView(conversationId);
      const tab = this.tabManager.createChatTab(
        conversationId,
        title ?? undefined,
      );
      this.tabManager.switchTo(tab.id);
      this.layout.switchTab("chat");
      this.chatPanel.switchToConversation(conversationId);
    };

    this.exposeTestHooks();
  }

  private exposeTestHooks(): void {
    const win = window as any;
    win.__test_dockConversation = (
      conversationId: number,
      zone: "left" | "right",
    ) => {
      const tab = this.tabManager.getTabByConversationId(conversationId);
      if (!tab || tab.kind !== "chat") return false;
      if (this.layout.hasDockedConversation(conversationId)) return false;

      const viewEl = this.chatPanel.detachView(conversationId);
      if (!viewEl) return false;

      this.layout.dockConversationView(conversationId, zone, viewEl, tab.title);
      this.tabManager.removeTab(tab.id);
      const activeTab = this.tabManager.getActiveTab();
      if (activeTab) {
        this.tabManager.onTabSwitch?.(activeTab);
      }
      return true;
    };

    win.__test_undockConversation = (conversationId: number) => {
      this.layout.onUndockConversation?.(conversationId);
    };

    win.__test_getDragGeometry = () => {
      const layout = this.layout as any;
      const tabBarRect = (
        this.tabManager as any
      ).tabBarEl.getBoundingClientRect();
      const rightZoneRect = layout.rightZone.el.getBoundingClientRect();
      const leftZoneRect = layout.leftZone.el.getBoundingClientRect();
      const centerRect = layout.centerEl.getBoundingClientRect();
      console.log("zxcv geometry", {
        tabBarRect,
        rightZoneRect,
        leftZoneRect,
        centerRect,
      });
      return { tabBarRect, rightZoneRect, leftZoneRect, centerRect };
    };

    win.__test_simulateDockDrag = (clientX: number, clientY: number) => {
      this.layout.beginTabDockDrag();
      this.layout.updateTabDockDrag(clientX, clientY);
      const zone = this.layout.endTabDockDrag();
      console.log("zxcv simulateDockDrag", { clientX, clientY, zone });
      return zone;
    };

    win.__test_spawnFeedbackAgent = async (
      filePath: string,
      lineContent: string,
      feedback: string,
    ) => {
      if (!this.diffPanel.onFeedbackSubmit) return null;

      // Open a file tab so the diff panel has visible content and the
      // refreshFileContent path can be exercised by the test.
      this.openFileViewerTab(lineContent, filePath);

      // Create a feedback box in the diff panel so the test can assert
      // on its visible status (spinner → checkmark transition).
      this.diffPanel.showFeedbackInput(0, 0, filePath, [lineContent]);
      const key = `${filePath}:0-0`;
      const box = (this.diffPanel as any).feedbackBoxes.get(key) as
        | {
            status: string;
            summary: string;
            conversationId: number | null;
            element: HTMLElement;
          }
        | undefined;
      if (box) {
        box.status = "progress";
        box.summary = "Starting...";
        (this.diffPanel as any).renderFeedbackProgress(box, feedback);
      }

      // Replicate the onFeedbackSubmit logic but set box.conversationId
      // before sendMessage so that synchronous mock events can find the box.
      const backendKind = this.resolveConversationBackend();
      const result = await createConversation(
        this.resolveWorkspaceRootsForBackend(backendKind),
        backendKind,
        true,
      );
      const id = result.conversation_id;
      this.conversationTydeSessionMap.set(id, result.session_id);
      this.chatPanel.setConversationBackendKind(id, backendKind);
      const fileName = filePath.split("/").pop() || filePath;
      this.registerConversation({
        conversationId: id,
        name: `Feedback: ${fileName}:1-1`,
        summary: "Applying feedback...",
        isTyping: true,
        createdAt: Date.now(),
        projectId: this.projectId,
      });
      this.eventRouter.registerFeedbackAgent(id, filePath);
      if (box) box.conversationId = id;

      const message = [
        `Apply the following feedback to file: ${filePath}`,
        "Lines 1-1:",
        "```",
        lineContent,
        "```",
        "",
        "Feedback:",
        feedback,
        "",
        "Apply these changes autonomously without asking questions.",
      ].join("\n");
      await sendMessage(id, message);

      return id;
    };
  }

  private wireComponentCallbacks(): void {
    this.gitPanel.onShowDiff = (diff, path, before, after) => {
      if (before !== undefined && after !== undefined) {
        this.openBeforeAfterDiffTab(before, after, path);
        return;
      }
      this.openDiffTab(diff, path);
    };
    this.gitPanel.onError = (message) => this.notifications.error(message);
    this.fileExplorer.onFileSelect = (content, path) =>
      this.openFileViewerTab(content, path);
    this.fileExplorer.onError = (message) => this.notifications.error(message);
    this.chatPanel.onOpenFileLink = (filePath, lineNumber) => {
      void this.openFileFromLinkedMessage(filePath, lineNumber).catch((err) => {
        this.notifications.error(`Failed to open file: ${String(err)}`);
      });
    };
    this.chatPanel.onViewDiff = (filePath, before, after) =>
      this.openBeforeAfterDiffTab(before, after, filePath);

    this.agentsPanel.onAgentClick = (agent) => {
      const focused = this.focusConversation(agent.conversationId, agent.name);
      if (!focused && agent.agentId) {
        this.onRuntimeAgentClick?.(agent);
      }
    };

    this.diffPanel.onFeedbackSubmit = async (
      filePath,
      startLine,
      endLine,
      lineContent,
      feedback,
    ) => {
      const backendKind = this.resolveConversationBackend();
      const result = await createConversation(
        this.resolveWorkspaceRootsForBackend(backendKind),
        backendKind,
        true,
      );
      const id = result.conversation_id;
      this.conversationTydeSessionMap.set(id, result.session_id);
      this.chatPanel.setConversationBackendKind(id, backendKind);

      const fileName = filePath.split("/").pop() || filePath;
      const agentName = `Feedback: ${fileName}:${startLine + 1}-${endLine + 1}`;

      this.registerConversation({
        conversationId: id,
        name: agentName,
        summary: "Applying feedback...",
        isTyping: true,
        createdAt: Date.now(),
        projectId: this.projectId,
      });
      this.eventRouter.registerFeedbackAgent(id, filePath);
      await this.applyDefaultSpawnProfile(id, backendKind);

      const message = [
        `Apply the following feedback to file: ${filePath}`,
        `Lines ${startLine + 1}-${endLine + 1}:`,
        "```",
        lineContent,
        "```",
        "",
        "Feedback:",
        feedback,
        "",
        "Apply these changes autonomously without asking questions.",
      ].join("\n");

      try {
        await sendMessage(id, message);
      } catch (err) {
        this.agentsPanel.updateAgent(id, {
          isTyping: false,
          hasError: true,
          summary: String(err),
        });
        this.eventRouter.unregisterFeedbackAgent(id);
        throw err;
      }

      return id;
    };

    this.eventRouter.onRefreshFile = async (filePath) => {
      await this.refreshFileContentPath(filePath);
    };
  }

  private startNewConversation(
    tabLabel?: string,
    backendOverride?: BackendKind,
    agentDefinitionId?: string,
  ): void {
    void this.createNewConversationTab(
      tabLabel,
      backendOverride,
      agentDefinitionId ? { agentDefinitionId } : undefined,
    ).catch((err) => {
      console.error("Failed to create conversation:", err);
      this.notifications.error(
        err instanceof Error ? err.message : "Failed to create conversation",
      );
    });
  }

  private resolveConversationBackend(
    preferredBackend?: BackendKind,
  ): BackendKind {
    const enabled = this.getWorkspaceEnabledBackends();
    if (enabled.length === 0) {
      throw new Error(
        "No backends are enabled. Enable at least one backend in Settings → Backends.",
      );
    }
    const preferred =
      preferredBackend ?? this.getWorkspaceDefaultBackend() ?? enabled[0];
    if (!enabled.includes(preferred)) {
      const backendLabels: Record<string, string> = {
        tycode: "Tycode",
        codex: "Codex",
        claude: "Claude",
        kiro: "Kiro",
        gemini: "Gemini",
      };
      const label = backendLabels[preferred] ?? preferred;
      this.notifications.warning(
        `${label} backend is disabled. Using ${backendLabels[enabled[0]] ?? "Tycode"}.`,
      );
      return enabled[0];
    }
    if (
      (preferred === "codex" ||
        preferred === "claude" ||
        preferred === "kiro" ||
        preferred === "gemini") &&
      this.resolveWorkspaceRootsForBackend(preferred).length === 0
    ) {
      if (this.mode === "orchestrator") {
        const backendLabel =
          preferred === "codex"
            ? "Codex"
            : preferred === "claude"
              ? "Claude"
              : preferred === "gemini"
                ? "Gemini"
                : "Kiro";
        this.notifications.warning(
          `${backendLabel} ${this.definitionLabel} chats require at least one open local project. Using Tycode.`,
        );
      } else {
        const backendLabel =
          preferred === "codex"
            ? "Codex"
            : preferred === "claude"
              ? "Claude"
              : preferred === "gemini"
                ? "Gemini"
                : "Kiro";
        this.notifications.warning(
          `${backendLabel} backend does not support remote SSH workspaces yet. Using Tycode.`,
        );
      }
      return "tycode";
    }
    return preferred;
  }

  private async startFileWatching(): Promise<void> {
    if (!this.workspacePath.trim()) return;
    try {
      if (this.fileChangeUnlisten === null) {
        this.fileChangeUnlisten = await onFileChanged((payload) => {
          if (this.root.style.display === "none") return;

          // Refresh open file tab if the changed path matches
          const tab = this.tabManager.getTabByFilePath(payload.path, "file");
          if (tab) {
            void this.refreshFileContentPath(payload.path);
          }

          // Debounced explorer + git refresh for any change under the workspace,
          // but ignore .git internals to avoid a feedback loop (our own git
          // status calls write to .git/index.lock etc., re-triggering the watcher).
          if (
            payload.path.startsWith(this.workspacePath) &&
            !payload.path.includes("/.git/")
          ) {
            this.scheduleExplorerRefresh();
            this.gitPanel.requestRefresh();
          }
        });
      }
      await this.syncFileWatchSubscriptions();
      await watchWorkspaceDir(this.workspacePath);
    } catch (err) {
      console.warn("Failed to start file watching:", err);
    }
  }

  private scheduleExplorerRefresh(): void {
    if (this.explorerRefreshTimer !== null) {
      clearTimeout(this.explorerRefreshTimer);
    }
    this.explorerRefreshTimer = setTimeout(() => {
      this.explorerRefreshTimer = null;
      void this.fileExplorer.refresh(true);
    }, 300);
  }

  private stopFileWatching(): void {
    if (this.fileChangeUnlisten) {
      this.fileChangeUnlisten();
      this.fileChangeUnlisten = null;
    }
    if (this.explorerRefreshTimer !== null) {
      clearTimeout(this.explorerRefreshTimer);
      this.explorerRefreshTimer = null;
    }
    this.watchedFilePaths.clear();
    void unwatchWorkspaceDir();
  }

  private async syncFileWatchSubscriptions(): Promise<void> {
    const start = perfNow();
    if (this.root.style.display === "none") return;

    const filePaths = new Set<string>();
    for (const tab of this.tabManager.getTabs()) {
      if (tab.kind !== "file") continue;
      if (tab.fileView !== "file") continue;
      if (!tab.filePath) continue;
      filePaths.add(tab.filePath);
    }

    if (this.areStringSetsEqual(filePaths, this.watchedFilePaths)) {
      logTabPerf(
        "WorkspaceView.syncFileWatchSubscriptions",
        perfNow() - start,
        {
          watchCount: filePaths.size,
          result: "unchanged",
        },
      );
      return;
    }

    let syncInvokeMs = 0;
    try {
      const syncStart = perfNow();
      await syncFileWatchPaths(Array.from(filePaths));
      syncInvokeMs = perfNow() - syncStart;
      this.watchedFilePaths = filePaths;
      logTabPerf(
        "WorkspaceView.syncFileWatchSubscriptions",
        perfNow() - start,
        {
          watchCount: filePaths.size,
          result: "synced",
          syncInvokeMs,
        },
      );
    } catch (err) {
      console.warn("Failed to sync file watch paths:", err);
      logTabPerf(
        "WorkspaceView.syncFileWatchSubscriptions",
        perfNow() - start,
        {
          watchCount: filePaths.size,
          result: "error",
          error: String(err),
        },
      );
    }
  }

  private areStringSetsEqual(a: Set<string>, b: Set<string>): boolean {
    if (a.size !== b.size) return false;
    for (const value of a) {
      if (!b.has(value)) return false;
    }
    return true;
  }

  private async refreshOpenFileTabs(): Promise<void> {
    const filePaths = new Set<string>();
    for (const tab of this.tabManager.getTabs()) {
      if (tab.kind !== "file") continue;
      if (tab.fileView !== "file") continue;
      if (!tab.filePath) continue;
      filePaths.add(tab.filePath);
    }
    await Promise.all(
      Array.from(filePaths).map((filePath) =>
        this.refreshFileContentPath(filePath),
      ),
    );
  }

  private async refreshFileContentPath(filePath: string): Promise<void> {
    const start = perfNow();
    if (this.fileRefreshInFlight.has(filePath)) return;
    this.fileRefreshInFlight.add(filePath);

    let status: "ok" | "error" = "ok";
    try {
      const result = await readFileContent(filePath);
      this.diffPanel.refreshFileContent(filePath, result.content);
    } catch (err) {
      status = "error";
      console.warn(`Failed to auto-refresh file: ${filePath}`, err);
    } finally {
      logTabPerf("WorkspaceView.refreshFileContentPath", perfNow() - start, {
        filePath,
        status,
      });
      this.fileRefreshInFlight.delete(filePath);
    }
  }

  private requestSessionsRefreshForStartedConversation(
    conversationId: number,
  ): void {
    if (this.sessionsRefreshRequestedForConversation.has(conversationId))
      return;
    this.sessionsRefreshRequestedForConversation.add(conversationId);
    void this.requestSessionsList(false);
  }

  private async handleConversationAgentAction(
    agent: AgentInfo,
    action: AgentCardAction,
  ): Promise<void> {
    if (action === "terminate") return;
    const conversationId = agent.conversationId;
    try {
      if (action === "interrupt") {
        await cancelConversation(conversationId);
        return;
      }
      const tab = this.tabManager.getTabByConversationId(conversationId);
      if (tab) this.tabManager.closeTab(tab.id);
      this.closeConversationPermanently(conversationId);
    } catch (err) {
      const actionLabel = action === "interrupt" ? "interrupt" : "remove";
      this.notifications.error(
        `Failed to ${actionLabel} conversation: ${String(err)}`,
      );
    }
  }

  private runtimeAgentToPanelInfo(agent: RuntimeAgent): AgentInfo {
    let name = agent.name.trim() || `Agent ${agent.agent_id}`;
    const existing = this.agentsPanel.getAgentByConversationId(
      agent.conversation_id,
    );
    if (existing?.name && (name === "Bridge" || name === "Conversation")) {
      name = existing.name;
      void renameAgent(agent.agent_id, name);
    }

    const base = {
      agentId: agent.agent_id,
      conversationId: agent.conversation_id,
      name,
      agentType: agent.agent_type,
      createdAt: agent.created_at_ms,
      projectId: this.projectId,
      parentAgentId: agent.parent_agent_id,
    };

    // First appearance — set initial state from the backend snapshot.
    // EventRouter will take over once chat events start arriving.
    if (!existing) {
      return {
        ...base,
        summary: agent.is_running ? "Running..." : "Completed",
        isTyping: agent.is_running,
        hasError: agent.last_error != null,
      };
    }

    // Existing agent — preserve isTyping/summary/hasError that EventRouter
    // maintains. Only override on lifecycle transitions where the backend
    // state disagrees with the card state.
    if (agent.is_running && !existing.isTyping) {
      // Start transition: backend says running but card shows idle.
      return {
        ...base,
        summary: agent.summary.trim() || "Running...",
        isTyping: true,
        hasError: false,
      };
    }

    if (!agent.is_running && existing.isTyping) {
      // Stop transition: backend says stopped but card shows running.
      // Handles terminate/cancel from MCP where no TypingStatusChanged=false follows.
      return {
        ...base,
        summary: agent.last_error ?? (agent.summary.trim() || "Completed"),
        isTyping: false,
        hasError: agent.last_error != null,
      };
    }

    return {
      ...base,
      summary: existing.summary,
      isTyping: existing.isTyping,
      hasError: existing.hasError,
    };
  }

  private closeConversationPermanently(
    conversationId: number,
    reportError: boolean = true,
  ): void {
    closeConversation(conversationId).catch((err) => {
      if (isConversationMissingError(err)) return;
      if (reportError) {
        this.notifications.error(`Failed to close conversation: ${err}`);
      }
    });
    this.disposeConversation(conversationId);
  }

  private disposeFailedResumeConversation(conversationId: number): void {
    const tab = this.tabManager.getTabByConversationId(conversationId);
    if (tab) {
      this.tabManager.closeTab(tab.id);
      return;
    }
    this.closeConversationPermanently(conversationId, false);
  }

  private registerConversation(agent: AgentInfo): void {
    this.conversationIds.add(agent.conversationId);
    this.agentsPanel.upsertAgent(agent);
    this.emitConversationIdsChanged();
  }

  private disposeConversation(conversationId: number): void {
    this.unregisterConversation(conversationId);
    this.chatPanel.removeConversation(conversationId);
    this.conversationSessionMap.delete(conversationId);
    this.conversationBackendKindMap.delete(conversationId);
  }

  private unregisterConversation(conversationId: number): void {
    this.conversationIds.delete(conversationId);
    this.agentsPanel.removeAgent(conversationId);
    this.eventRouter.unregisterFeedbackAgent(conversationId);
    this.conversationTydeSessionMap.delete(conversationId);
    this.titleGenerationRequested.delete(conversationId);
    this.sessionsRefreshRequestedForConversation.delete(conversationId);
    this.emitConversationIdsChanged();
  }

  private findConversationBySession(
    sessionId: string,
    backendKind: BackendKind,
  ): number | null {
    for (const [conversationId, info] of this.conversationSessionMap) {
      if (
        info.sessionId === sessionId &&
        info.backendKind === backendKind &&
        this.conversationIds.has(conversationId)
      ) {
        return conversationId;
      }
    }
    return null;
  }

  private emitConversationIdsChanged(): void {
    this.onConversationIdsChange?.(Array.from(this.conversationIds));
  }

  updateServerConnectionState(payload: {
    host_id: string;
    state:
      | string
      | { reconnecting: { attempt: number } }
      | { disconnected: { reason: string } };
  }): void {
    // Only show for this workspace's host
    const remote = parseRemoteWorkspaceUri(this.workspacePath);
    if (!remote) return;
    if (this.resolvedHostId && payload.host_id !== this.resolvedHostId) return;

    const el = this.serverConnStatusEl;
    if (!el) return;

    const state = payload.state;
    if (typeof state === "string") {
      if (state === "connected") {
        el.style.display = "flex";
        el.className = "tyde-server-conn-status connected";
        el.textContent = "Server Connected";
      } else if (state === "connecting") {
        el.style.display = "flex";
        el.className = "tyde-server-conn-status connecting";
        el.textContent = "Connecting...";
      }
    } else if ("reconnecting" in state) {
      el.style.display = "flex";
      el.className = "tyde-server-conn-status reconnecting";
      el.textContent = `Reconnecting (${state.reconnecting.attempt})...`;
    } else if ("disconnected" in state) {
      el.style.display = "flex";
      el.className = "tyde-server-conn-status disconnected";
      el.textContent = "Disconnected";
    }
  }
}
