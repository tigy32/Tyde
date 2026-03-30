import type { ChatEvent } from "@tyde/protocol";
import type { AgentCardAction, AgentInfo } from "./agents";
import type {
  AdminEventPayload,
  ChatEventPayload,
  ConversationRegisteredPayload,
  CreateWorkbenchEventPayload,
  DeleteWorkbenchEventPayload,
  Host,
  McpHttpServerSettings,
  RuntimeAgent,
} from "./bridge";
import {
  addHost,
  addRecentWorkspace,
  cancelConversation,
  closeConversation,
  getInitialWorkspace,
  gitWorktreeAdd,
  gitWorktreeRemove,
  installBackendDependency as installBackendDependencyBridge,
  interruptAgent,
  listAgents,
  listHosts,
  onAdminEvent,
  onAgentChanged,
  onChatEvent,
  onCreateWorkbench,
  onDeleteWorkbench,
  openWorkspaceDialog,
  pickSubRootDialog,
  terminateAgent,
} from "./bridge";
import { CommandPalette } from "./command_palette";
import { ConnectionDialog } from "./connection_dialog";
import { registerDebugUiBridge } from "./debug_ui_bridge";
import { showFeedbackDialog } from "./feedback_dialog";
import { HomeView } from "./home_view";
import {
  EscapeStack,
  formatShortcut,
  KeyboardManager,
  setCheatSheetEscapeStack,
  showCheatSheet,
} from "./keyboard";
import { NotificationManager } from "./notifications";
import { ProjectStateManager } from "./project_state";
import { ProjectSidebar } from "./projects";
import { RemoteBrowserDialog } from "./remote_browser_dialog";
import {
  adjustFontSize,
  initializeBackendDependencies,
  needsTycodeUpgrade,
  SettingsPanel,
} from "./settings";
import { promptForText } from "./text_prompt";
import {
  normalizeRemoteWorkspaceInput,
  parseRemoteWorkspaceUri,
  workspaceDisplayName,
} from "./workspace";
import { WorkspaceView } from "./workspace_view";

const HOME_BRIDGE_VIEW_ID = "__home_bridge__";
const HOME_BRIDGE_LABEL = "Bridge";
const INTERNAL_AGENT_PREFIX = "__internal_";

export class AppController {
  private notifications!: NotificationManager;
  private keyboard!: KeyboardManager;
  private commandPalette!: CommandPalette;
  private settingsPanel!: SettingsPanel;
  private homeView!: HomeView;
  private projectSidebar!: ProjectSidebar;
  private projectState!: ProjectStateManager;
  private connectionDialog = new ConnectionDialog();
  private settingsTabViewEl!: HTMLElement;
  private escapeStack!: EscapeStack;
  private bridgeControlEnabled = true;
  private runtimeAgents = new Map<number, RuntimeAgent>();
  private runtimeAgentsByProjectId = new Map<string, RuntimeAgent[]>();
  private hiddenRuntimeAgentIds = new Set<number>();
  private registeredWorkflowCommandIds: string[] = [];

  private workspaceViews = new Map<string, WorkspaceView>();
  private activeWorkspaceId: string | null = null;
  private homeBridgeView!: WorkspaceView;

  constructor() {
    this.initializeTheme();
    this.buildSharedComponents();
  }

  async init(): Promise<void> {
    this.registerCommands();
    this.registerKeyboardShortcuts();
    this.wireDockButtons();

    await onChatEvent(
      (registered) => this.handleConversationRegistered(registered),
      (payload) => this.routeChatEvent(payload),
    );
    await onAdminEvent((payload) => this.routeAdminEvent(payload));
    await registerDebugUiBridge();
    await onCreateWorkbench((payload) => {
      this.handleCreateWorkbench(payload);
    });
    await onDeleteWorkbench((payload) => {
      void this.handleDeleteWorkbench(payload);
    });
    document
      .getElementById("open-workspace-btn")!
      .addEventListener("click", () => this.openWorkspace());
    document
      .getElementById("open-remote-workspace-btn")!
      .addEventListener("click", () => this.openRemoteWorkspace());
    document
      .getElementById("feedback-btn")!
      .addEventListener("click", () => showFeedbackDialog());
    document
      .getElementById("header-settings-btn")!
      .addEventListener("click", () => this.openSettings());

    await initializeBackendDependencies();
    this.refreshAllBackendMenus();
    await this.reconcileRustHostsWithProjects();

    // If the user previously enabled tycode but the binary is missing after an
    // app update, kick off a background install and notify. Don't block startup.
    if (needsTycodeUpgrade()) {
      this.notifications.info(
        "Tycode update required — installing new version in the background.",
      );
      installBackendDependencyBridge("tycode")
        .then(() => {
          this.settingsPanel.refreshBackendDependencies();
          this.refreshAllBackendMenus();
          this.notifications.success("Tycode updated successfully.");
        })
        .catch((err) => {
          console.error("[tycode] Background install failed:", err);
          this.notifications.error(
            `Tycode update failed: ${String(err)}. You can retry from Settings → Backends.`,
          );
        });
    }

    await onAgentChanged((agent) => {
      this.runtimeAgents.set(agent.agent_id, agent);
      this.applyRuntimeAgents(Array.from(this.runtimeAgents.values()));
    });

    // Seed agent map with current state at startup
    const initialAgents = await listAgents();
    this.runtimeAgents = new Map(
      initialAgents.map((agent) => [agent.agent_id, agent]),
    );
    this.applyRuntimeAgents(initialAgents);

    await this.bootstrapStartup();

    const splash = document.getElementById("splash");
    if (splash) {
      splash.classList.add("fade-out");
      splash.addEventListener("transitionend", () => splash.remove());
    }
  }

  showError(msg: string): void {
    this.notifications.error(msg);
  }

  persistActiveProjectUiState(): void {}

  private refreshAllBackendMenus(): void {
    for (const view of this.workspaceViews.values()) {
      view.refreshNewChatMenu();
    }
    this.homeView.render();
  }

  private getActiveView(): WorkspaceView | null {
    if (!this.activeWorkspaceId) return null;
    return this.workspaceViews.get(this.activeWorkspaceId) ?? null;
  }

  private getWorkspaceViewForHost(host: Host): WorkspaceView | null {
    for (const project of this.projectState.projects) {
      const view = this.workspaceViews.get(project.id);
      if (!view) continue;
      const remote = parseRemoteWorkspaceUri(project.workspacePath);
      if (host.is_local && !remote) return view;
      if (!host.is_local && remote?.host === host.hostname) return view;
    }
    if (host.is_local) return this.homeBridgeView;
    return null;
  }

  private async handleSettingsHostChange(host: Host | null): Promise<void> {
    if (!host) {
      this.settingsPanel.adminId = null;
      return;
    }
    const view = this.getWorkspaceViewForHost(host);
    if (!view) {
      this.settingsPanel.adminId = null;
      return;
    }
    try {
      const adminId = await view.ensureAdminSubprocess("tycode");
      this.settingsPanel.adminId = adminId;
    } catch (err) {
      console.error("Failed to initialize admin subprocess for host:", err);
      this.settingsPanel.adminId = null;
    }
  }

  private getViewByAdminId(adminId: number): WorkspaceView | null {
    for (const view of this.workspaceViews.values()) {
      if (view.ownsAdminId(adminId)) return view;
    }
    return null;
  }

  private initializeTheme(): void {
    const root = document.documentElement;
    const stored = localStorage.getItem("tyde-theme");
    if (stored === "light" || stored === "dark") {
      root.dataset.theme = stored === "light" ? "light" : "";
      return;
    }
    const prefersDark = window.matchMedia(
      "(prefers-color-scheme: dark)",
    ).matches;
    root.dataset.theme = prefersDark ? "" : "light";
  }

  private buildSharedComponents(): void {
    window
      .matchMedia("(prefers-color-scheme: dark)")
      .addEventListener("change", (e) => {
        if (localStorage.getItem("tyde-theme")) return;
        document.documentElement.dataset.theme = e.matches ? "" : "light";
      });

    this.projectState = new ProjectStateManager();

    this.notifications = new NotificationManager();
    this.notifications.requestPermission();
    const bellContainer = document.getElementById("notification-bell")!;
    bellContainer.appendChild(this.notifications.createBellButton());

    this.commandPalette = new CommandPalette();
    this.commandPalette.onError = (msg) => this.notifications.error(msg);

    this.escapeStack = new EscapeStack();
    this.keyboard = new KeyboardManager(this.escapeStack);
    setCheatSheetEscapeStack(this.escapeStack);

    const originalShow = this.commandPalette.show.bind(this.commandPalette);
    const originalHide = this.commandPalette.hide.bind(this.commandPalette);
    this.commandPalette.show = () => {
      originalShow();
      this.escapeStack.push("command-palette", () =>
        this.commandPalette.hide(),
      );
    };
    this.commandPalette.hide = () => {
      originalHide();
      this.escapeStack.remove("command-palette");
    };

    const settingsTabViewEl = document.createElement("div");
    settingsTabViewEl.className = "settings-tab-view hidden";
    settingsTabViewEl.dataset.testid = "settings-tab-view";
    this.settingsTabViewEl = settingsTabViewEl;
    this.settingsPanel = new SettingsPanel(settingsTabViewEl);
    this.settingsPanel.onClose = () => this.closeSettings();
    this.settingsPanel.onBackendsChanged = () => {
      const hostRefreshes = Array.from(this.workspaceViews.values()).map(
        (view) => view.refreshHostSettings(),
      );
      void Promise.all(hostRefreshes).then(() => this.refreshAllBackendMenus());
    };
    this.settingsPanel.onHostChange = (host) => {
      void this.handleSettingsHostChange(host);
    };
    this.settingsPanel.onHostsUpdated = () => {
      if (this.projectSidebar) this.projectSidebar.refreshHosts();
      if (this.homeView) this.homeView.refreshHosts();
      const hostRefreshes = Array.from(this.workspaceViews.values()).map(
        (view) => view.refreshHostSettings(),
      );
      void Promise.all(hostRefreshes).then(() => this.refreshAllBackendMenus());
    };

    settingsTabViewEl.classList.add("settings-overlay");
    const container = document.getElementById("workspace-container")!;
    container.appendChild(settingsTabViewEl);
    this.homeBridgeView = new WorkspaceView({
      projectId: HOME_BRIDGE_VIEW_ID,
      workspacePath: "",
      projectName: HOME_BRIDGE_LABEL,
      notifications: this.notifications,
      mode: "bridge",
      bridgeChatLabel: HOME_BRIDGE_LABEL,
      bridgeChatEnabled: this.bridgeControlEnabled,
      bridgeChatDisabledReason: this.bridgeChatDisabledReason(),
      getBridgeProjects: () =>
        this.projectState.projects.map((project) => ({
          name: project.name,
          workspacePath: project.workspacePath,
          roots: project.roots,
        })),
      availableWidgets: ["sessions", "agents"],
    });
    this.homeBridgeView.root.style.display = "none";
    container.appendChild(this.homeBridgeView.root);
    this.workspaceViews.set(HOME_BRIDGE_VIEW_ID, this.homeBridgeView);

    const homeViewEl = document.createElement("div");
    homeViewEl.className = "home-view-container";
    this.homeBridgeView.getHomeViewContainer()?.appendChild(homeViewEl);

    this.homeView = new HomeView(homeViewEl, this.projectState);
    this.homeView.onOpenWorkspace = () => this.openWorkspace();
    this.homeView.onOpenRemoteWorkspace = () => this.openRemoteWorkspace();
    this.homeView.onNewBridgeChat = (backendOverride) => {
      // Ensure the bridge view is the active workspace before creating the
      // conversation.  This is normally already the case (the button lives in
      // HomeView which is inside homeBridgeView), but an explicit switch
      // prevents races where async operations during conversation creation
      // could cause a project view to become active.
      if (this.activeWorkspaceId !== HOME_BRIDGE_VIEW_ID) {
        this.switchToHome();
      }
      void this.homeBridgeView
        .createNewConversationTab(undefined, backendOverride)
        .catch((err) => {
          console.error("Failed to create bridge conversation:", err);
          this.notifications.error(
            err instanceof Error
              ? err.message
              : "Failed to create bridge conversation",
          );
        });
    };
    this.homeView.onSwitchProject = (id) => this.switchToWorkspace(id);
    this.homeView.resolveProjectAgentCounts = (projectId) =>
      this.getProjectAgentCounts(projectId);
    this.homeView.resolveAllAgents = async () => {
      return Array.from(this.runtimeAgents.values()).filter((agent) =>
        this.shouldDisplayRuntimeAgent(agent),
      );
    };
    this.homeView.onAgentAction = (agent, action) => {
      void this.handleRuntimeAgentAction(agent, action);
    };
    this.homeView.onAgentClick = (agent) => {
      this.openRuntimeAgentInWorkspace(agent);
    };
    this.homeView.setBridgeChatAvailability(
      this.bridgeControlEnabled,
      this.bridgeChatDisabledReason(),
      HOME_BRIDGE_LABEL,
    );
    this.homeBridgeView.onRuntimeAgentAction = (agent, action) => {
      void this.handleRuntimeAgentAction(agent, action);
    };
    this.homeBridgeView.onRuntimeAgentClick = (agent) => {
      const runtimeAgent = agent.agentId
        ? this.runtimeAgents.get(agent.agentId)
        : undefined;
      if (!runtimeAgent) return;
      this.openRuntimeAgentInWorkspace(runtimeAgent);
    };
    this.settingsPanel.onMcpHttpSettingsChange = (settings) => {
      this.handleBridgeControlSettingsChange(settings);
    };
    this.settingsPanel.notifySelectedHostChanged();

    const projectRail = document.getElementById("project-rail")!;
    const sidebar = new ProjectSidebar(
      projectRail,
      this.projectState,
      (id: string) => this.switchToWorkspace(id),
      () => this.switchToHome(),
      () => this.handleAddProject(),
      (id) => this.handleRemoveProject(id),
    );
    this.projectSidebar = sidebar;
    sidebar.onCreateWorkbench = (parentId) => {
      void this.createWorkbench(parentId);
    };
    sidebar.onRemoveWorkbench = (projectId) => {
      void this.removeWorkbench(projectId);
    };
    sidebar.onManageRoots = (projectId) => {
      void this.showManageRootsDialog(projectId);
    };
    sidebar.onAddRemoteProject = (host) => {
      const dialog = new RemoteBrowserDialog(host, (sshUri) => {
        void this.openWorkspacePath(sshUri);
      });
      dialog.show();
    };

    const sidebarOnChange = this.projectState.onChange;
    this.projectState.onChange = () => {
      sidebarOnChange?.();
      this.applyRuntimeAgents(Array.from(this.runtimeAgents.values()));
      this.homeView.render();
    };

    const sidePanel = document.getElementById("side-panel");
    if (sidePanel) {
      sidePanel.classList.remove("visible");
      (sidePanel as HTMLElement).style.display = "none";
    }

    this.commandPalette.onFileSelect = (content, path) => {
      const view = this.getActiveView();
      if (view) view.openFileViewerTab(content, path);
    };
  }

  private getOrCreateWorkspaceView(
    projectId: string,
    workspacePath: string,
    projectName: string,
  ): WorkspaceView {
    const existing = this.workspaceViews.get(projectId);
    if (existing) return existing;

    const project = this.projectState.projects.find((p) => p.id === projectId);
    const view = new WorkspaceView({
      projectId,
      workspacePath,
      projectName,
      notifications: this.notifications,
      roots: project?.roots,
    });

    const viewContainer = document.getElementById("workspace-container")!;
    viewContainer.appendChild(view.root);

    view.onConversationIdsChange = (conversationIds) => {
      this.projectState.setProjectConversationIds(projectId, conversationIds);
    };
    view.onAgentsChange = () => {
      if (this.projectState.isHomeActive()) {
        this.homeView.render();
      }
    };
    view.onRuntimeAgentAction = (agent, action) => {
      void this.handleRuntimeAgentAction(agent, action);
    };
    view.onWorkflowsChanged = () => {
      this.registerWorkflowCommands(view);
    };
    this.projectState.setProjectConversationIds(
      projectId,
      view.getConversationIds(),
    );
    view.syncRuntimeAgents(this.runtimeAgentsByProjectId.get(projectId) ?? []);

    this.workspaceViews.set(projectId, view);
    return view;
  }

  private getProjectAgentCounts(projectId: string): {
    total: number;
    active: number;
  } {
    const view = this.workspaceViews.get(projectId);
    if (!view) {
      const project = this.projectState.projects.find(
        (p) => p.id === projectId,
      );
      const runtimeAgents = this.runtimeAgentsByProjectId.get(projectId) ?? [];
      const runtimeConversationIds = new Set(
        runtimeAgents.map((agent) => agent.conversation_id),
      );
      const persistedConversationIds = project?.conversationIds ?? [];
      const totalConversationIds = new Set<number>(persistedConversationIds);
      for (const conversationId of runtimeConversationIds) {
        totalConversationIds.add(conversationId);
      }
      const activeRuntimeAgents = runtimeAgents.filter(
        (agent) => agent.is_running,
      ).length;
      const activeConversationIds = persistedConversationIds.filter(
        (id) => !runtimeConversationIds.has(id),
      ).length;
      return {
        total: totalConversationIds.size,
        active: activeConversationIds + activeRuntimeAgents,
      };
    }
    const agents = view.getAgentsPanel().getAgents();
    const total = agents.length;
    const active = agents.filter((agent) => agent.isTyping).length;
    return { total, active };
  }

  private applyRuntimeAgents(agents: RuntimeAgent[]): void {
    const visibleAgents = agents.filter((agent) =>
      this.shouldDisplayRuntimeAgent(agent),
    );
    const byProjectId = new Map<string, RuntimeAgent[]>();

    for (const project of this.projectState.projects) {
      byProjectId.set(project.id, []);
    }

    for (const agent of visibleAgents) {
      const project = this.resolveProjectForRuntimeAgent(agent);
      if (!project) continue;
      const bucket = byProjectId.get(project.id) ?? [];
      bucket.push(agent);
      byProjectId.set(project.id, bucket);
    }

    this.runtimeAgentsByProjectId = byProjectId;
    this.homeView.setAgents(visibleAgents);

    for (const project of this.projectState.projects) {
      const view = this.workspaceViews.get(project.id);
      if (!view) continue;
      view.syncRuntimeAgents(byProjectId.get(project.id) ?? []);
    }

    this.homeBridgeView.syncRuntimeAgentPreviews(visibleAgents);
  }

  private openRuntimeAgentInWorkspace(agent: RuntimeAgent): void {
    const project = this.resolveProjectForRuntimeAgent(agent);
    if (!project) return;
    this.switchToWorkspace(project.id);
    const view = this.workspaceViews.get(project.id);
    if (!view) return;
    view.syncRuntimeAgent(agent);
    view.focusConversation(agent.conversation_id, agent.name);
  }

  private resolveProjectForRuntimeAgent(
    agent: RuntimeAgent,
  ): { id: string; workspacePath: string } | null {
    for (const [projectId, view] of this.workspaceViews) {
      if (projectId === HOME_BRIDGE_VIEW_ID) {
        // If the bridge view owns this conversation, it should stay there —
        // don't resolve to a project view (which would cause dual-ownership).
        if (view.ownsConversation(agent.conversation_id)) return null;
        continue;
      }
      if (!view.ownsConversation(agent.conversation_id)) continue;
      const project = this.projectState.projects.find(
        (entry) => entry.id === projectId,
      );
      if (project) return project;
    }

    return (
      this.resolveProjectForWorkspaceRoots(agent.workspace_roots) ??
      this.resolveProjectByParentAgent(agent.parent_agent_id) ??
      this.resolveProjectByActiveView()
    );
  }

  private shouldDisplayRuntimeAgent(agent: RuntimeAgent): boolean {
    if (this.hiddenRuntimeAgentIds.has(agent.agent_id)) return false;

    const name = agent.name.trim();
    if (name.startsWith(INTERNAL_AGENT_PREFIX)) return false;

    // Backward compatibility for older title helpers created before the internal prefix.
    if (/^title\s+\d+$/i.test(name)) return false;

    return true;
  }

  private async handleRuntimeAgentAction(
    agent: RuntimeAgent | AgentInfo,
    action: AgentCardAction,
  ): Promise<void> {
    const agentId = "agent_id" in agent ? agent.agent_id : agent.agentId;
    if (!agentId) return;

    try {
      if (action === "interrupt") {
        await interruptAgent(agentId);
        return;
      }
      if (action === "terminate") {
        await terminateAgent(agentId);
        this.closeRuntimeAgentTab(agent);
        return;
      }

      this.closeRuntimeAgentTab(agent);
      this.hiddenRuntimeAgentIds.add(agentId);
      this.applyRuntimeAgents(Array.from(this.runtimeAgents.values()));
    } catch (err) {
      const actionLabel =
        action === "interrupt"
          ? "interrupt"
          : action === "terminate"
            ? "terminate"
            : "remove";
      this.notifications.error(
        `Failed to ${actionLabel} agent: ${String(err)}`,
      );
    }
  }

  private closeRuntimeAgentTab(agent: RuntimeAgent | AgentInfo): void {
    const conversationId =
      "conversation_id" in agent ? agent.conversation_id : agent.conversationId;
    for (const view of this.workspaceViews.values()) {
      const tabManager = view.getTabManager();
      const tab = tabManager.getTabByConversationId(conversationId);
      if (tab) {
        tabManager.closeTab(tab.id);
        return;
      }
    }
  }

  private bridgeChatDisabledReason(): string {
    return "Enable Loopback MCP Control in Settings to start Bridge chats.";
  }

  private handleBridgeControlSettingsChange(
    settings: McpHttpServerSettings,
  ): void {
    this.bridgeControlEnabled = settings.enabled;
    const reason = this.bridgeChatDisabledReason();
    this.homeView.setBridgeChatAvailability(
      settings.enabled,
      reason,
      HOME_BRIDGE_LABEL,
    );
    this.homeBridgeView.updateNewConversationAvailability(
      settings.enabled,
      reason,
    );
  }

  private switchToWorkspace(projectId: string): void {
    if (this.activeWorkspaceId) {
      const current = this.workspaceViews.get(this.activeWorkspaceId);
      current?.hide();
    }

    const project = this.projectState.projects.find((p) => p.id === projectId);
    if (!project) return;

    const view = this.getOrCreateWorkspaceView(
      project.id,
      project.workspacePath,
      project.name,
    );
    view.show();
    this.activeWorkspaceId = projectId;

    this.projectState.switchProject(projectId);
    this.commandPalette.setWorkspaceRoot(project.workspacePath);
    document.querySelector(".app-title")!.textContent =
      `Tyde — ${project.name}`;

    this.registerWorkflowCommands(view);
    this.homeView.hide();
    this.homeBridgeView.hide();
    this.settingsTabViewEl.classList.add("hidden");
  }

  private switchToHome(): void {
    if (this.activeWorkspaceId) {
      const current = this.workspaceViews.get(this.activeWorkspaceId);
      current?.hide();
    }
    this.activeWorkspaceId = HOME_BRIDGE_VIEW_ID;
    this.projectState.switchToHome();
    this.homeBridgeView.show();
    this.homeView.show();
    this.commandPalette.setWorkspaceRoot("");
    this.settingsTabViewEl.classList.add("hidden");
    document.querySelector(".app-title")!.textContent = "Tyde";
  }

  private handleConversationRegistered(
    payload: ConversationRegisteredPayload,
  ): void {
    // If a view already owns this conversation (e.g. createNewConversationTab
    // registered it before this event arrived), route to that view to avoid
    // dual-ownership that sends events to a hidden workspace view.
    let view: WorkspaceView | undefined;
    for (const v of this.workspaceViews.values()) {
      if (v.ownsConversation(payload.conversation_id)) {
        view = v;
        break;
      }
    }

    if (!view) {
      // Top-level conversations created while the bridge view is active belong
      // to the bridge view.  Resolve early to avoid getOrCreateWorkspaceView
      // side-effects (e.g. syncing agents onto a project view, causing
      // dual-ownership).
      if (
        payload.data.parent_agent_id == null &&
        this.activeWorkspaceId === HOME_BRIDGE_VIEW_ID
      ) {
        view = this.homeBridgeView;
      } else {
        let project = this.resolveProjectForWorkspaceRoots(
          payload.data.workspace_roots,
        );
        if (!project) {
          project = this.resolveProjectByParentAgent(
            payload.data.parent_agent_id,
          );
        }
        if (!project) {
          project = this.resolveProjectByActiveView();
        }
        if (project) {
          view = this.getOrCreateWorkspaceView(
            project.id,
            project.workspacePath,
            project.name,
          );
        } else {
          view = this.homeBridgeView;
        }
      }
    }

    const agent: RuntimeAgent = {
      agent_id: payload.data.agent_id ?? 0,
      conversation_id: payload.conversation_id,
      workspace_roots: payload.data.workspace_roots,
      backend_kind: payload.data.backend_kind,
      parent_agent_id: payload.data.parent_agent_id,
      name: payload.data.name,
      agent_type: payload.data.agent_type ?? null,
      is_running: true,
      summary: "",
      created_at_ms: Date.now(),
      updated_at_ms: Date.now(),
      ended_at_ms: null,
      last_error: null,
      last_message: null,
    };
    view.syncRuntimeAgent(agent);
  }

  private resolveProjectForWorkspaceRoots(
    workspaceRoots: string[],
  ): { id: string; workspacePath: string; name: string } | null {
    let bestMatch: { id: string; workspacePath: string; name: string } | null =
      null;
    let bestLength = -1;
    for (const project of this.projectState.projects) {
      const normalizedWorkspace = this.normalizeWorkspacePath(
        project.workspacePath,
      );
      if (!normalizedWorkspace) continue;
      const matches = workspaceRoots.some((root) => {
        const normalizedRoot = this.normalizeWorkspacePath(root);
        if (!normalizedRoot) return false;
        return (
          normalizedRoot === normalizedWorkspace ||
          normalizedRoot.startsWith(`${normalizedWorkspace}/`) ||
          normalizedWorkspace.startsWith(`${normalizedRoot}/`)
        );
      });
      if (!matches) continue;
      if (project.workspacePath.length <= bestLength) continue;
      bestMatch = project;
      bestLength = project.workspacePath.length;
    }
    return bestMatch;
  }

  private resolveProjectByParentAgent(
    parentAgentId: number | null | undefined,
  ): { id: string; workspacePath: string; name: string } | null {
    if (parentAgentId == null) return null;
    const parent = this.runtimeAgents.get(parentAgentId);
    if (!parent) return null;
    for (const [projectId, view] of this.workspaceViews) {
      if (projectId === HOME_BRIDGE_VIEW_ID) continue;
      if (!view.ownsConversation(parent.conversation_id)) continue;
      const project = this.projectState.projects.find(
        (entry) => entry.id === projectId,
      );
      if (project) return project;
    }
    return null;
  }

  private resolveProjectByActiveView(): {
    id: string;
    workspacePath: string;
    name: string;
  } | null {
    if (
      !this.activeWorkspaceId ||
      this.activeWorkspaceId === HOME_BRIDGE_VIEW_ID
    ) {
      return null;
    }
    return (
      this.projectState.projects.find((p) => p.id === this.activeWorkspaceId) ??
      null
    );
  }

  private normalizeWorkspacePath(path: string): string {
    return path.replace(/\\/g, "/").replace(/\/+$/, "");
  }

  private routeChatEvent(payload: ChatEventPayload): void {
    if (this.tryRouteChatEvent(payload)) {
      return;
    }
    console.error(
      `routeChatEvent: no workspace view owns conversation ${payload.conversation_id}. ` +
        `ConversationRegistered event may not have been received.`,
    );
  }

  private tryRouteChatEvent(payload: ChatEventPayload): boolean {
    for (const view of this.workspaceViews.values()) {
      if (!view.ownsConversation(payload.conversation_id)) continue;
      view.handleChatEvent(payload);
      if (view.projectId !== HOME_BRIDGE_VIEW_ID) {
        this.updateProjectStatusFromEvent(view.projectId, payload.event);
      }
      return true;
    }
    return false;
  }

  private updateProjectStatusFromEvent(
    projectId: string,
    event: ChatEvent,
  ): void {
    if (event.kind === "StreamStart") {
      this.projectState.updateProjectStatus(projectId, "active");
      return;
    }
    if (event.kind === "StreamEnd") {
      this.projectState.updateProjectStatus(projectId, "idle");
      return;
    }
    if (event.kind === "SubprocessExit") {
      this.projectState.updateProjectStatus(projectId, "idle");
      return;
    }
  }

  private routeAdminEvent(payload: AdminEventPayload): void {
    const event = payload.event;
    if (event.kind === "SubprocessStderr") {
      console.warn(`[admin:${payload.admin_id}] stderr:`, event.data);
    }
    if (event.kind === "SubprocessExit") {
      console.warn(
        `[admin:${payload.admin_id}] exited with code:`,
        event.data.exit_code,
      );
    }
    const sourceView = this.getViewByAdminId(payload.admin_id);
    if (event.kind === "Settings") {
      if (this.settingsPanel.adminId !== payload.admin_id) return;
      this.settingsPanel.handleSettingsData(event.data);
      return;
    }
    if (event.kind === "ModuleSchemas") {
      if (this.settingsPanel.adminId !== payload.admin_id) return;
      this.settingsPanel.handleModuleSchemas(event.data.schemas);
      return;
    }
    if (event.kind === "ProfilesList") {
      if (this.settingsPanel.adminId !== payload.admin_id) return;
      this.settingsPanel.handleProfilesList(event.data);
      return;
    }
    if (event.kind === "SubprocessExit") {
      if (sourceView) sourceView.handleAdminEvent(payload);
      return;
    }
    if (event.kind === "SessionsList") {
      if (sourceView) {
        sourceView.handleAdminEvent(payload);
        return;
      }
      if (this.activeWorkspaceId) {
        const view = this.workspaceViews.get(this.activeWorkspaceId);
        if (view) view.handleAdminEvent(payload);
      }
    }
  }

  private async openWorkspacePath(dir: string): Promise<void> {
    const remote = parseRemoteWorkspaceUri(dir);
    if (remote) {
      await this.ensureRemoteHostRegistered(remote.host);
      await this.connectionDialog.show(remote.host);
      this.settingsPanel.refreshHosts();
      if (this.projectSidebar) this.projectSidebar.refreshHosts();
      if (this.homeView) this.homeView.refreshHosts();
    }

    const displayName = workspaceDisplayName(dir);

    let project = this.projectState.projects.find(
      (p) => p.workspacePath === dir,
    );

    if (remote && !project) {
      // For new remote workspaces, create an unpersisted project so we can
      // build the view and attempt connection. Only persist after success.
      project = this.projectState.createProject(dir);
      project.name = displayName;

      const view = this.getOrCreateWorkspaceView(
        project.id,
        project.workspacePath,
        project.name,
      );
      this.switchToWorkspace(project.id);

      let connected = false;
      try {
        await view.createNewConversationTab();
        connected = true;
      } finally {
        if (!connected) {
          view.destroy();
          view.root.remove();
          this.workspaceViews.delete(project.id);
          this.projectState.abandonProject(project.id);
          this.switchToHome();
        }
      }

      // Connection succeeded — persist
      this.projectState.commitProject(project);
    } else {
      if (!project) {
        project = this.projectState.addProject(dir);
        project.name = displayName;
      }

      const view = this.getOrCreateWorkspaceView(
        project.id,
        project.workspacePath,
        project.name,
      );
      this.switchToWorkspace(project.id);

      if (remote) {
        await view.createNewConversationTab();
      }
    }

    addRecentWorkspace(dir);
  }

  private async ensureRemoteHostRegistered(hostname: string): Promise<void> {
    try {
      const hosts = await listHosts();
      if (hosts.some((host) => host.hostname === hostname)) return;
      await addHost(hostname, hostname);
    } catch (err) {
      console.error("Failed to register remote host:", err);
    }
  }

  private async reconcileRustHostsWithProjects(): Promise<void> {
    try {
      const hosts = await listHosts();
      const known = new Set(hosts.map((host) => host.hostname));
      const remoteHosts = new Set<string>();
      for (const project of this.projectState.projects) {
        const remote = parseRemoteWorkspaceUri(project.workspacePath);
        if (!remote) continue;
        remoteHosts.add(remote.host);
      }
      for (const remoteHost of remoteHosts) {
        if (known.has(remoteHost)) continue;
        await addHost(remoteHost, remoteHost);
      }
      this.settingsPanel.refreshHosts();
      if (this.projectSidebar) this.projectSidebar.refreshHosts();
      if (this.homeView) this.homeView.refreshHosts();
    } catch (err) {
      console.error("Failed to reconcile hosts from projects:", err);
    }
  }

  private async openWorkspace(): Promise<void> {
    const dir = await openWorkspaceDialog();
    if (!dir) return;
    await this.openWorkspacePath(dir);
  }

  private async openRemoteWorkspace(): Promise<void> {
    const raw = await promptForText({
      title: "Open Remote Workspace",
      description: "Enter user@host:/absolute/path or ssh://user@host/path",
      placeholder: "user@host:/absolute/path",
      confirmLabel: "Open",
    });
    if (raw === null) return;

    const remoteUri = normalizeRemoteWorkspaceInput(raw);
    if (!remoteUri) {
      this.notifications.error(
        "Invalid remote workspace format. Use user@host:/path.",
      );
      return;
    }

    await this.openWorkspacePath(remoteUri);
  }

  private async handleAddProject(): Promise<void> {
    const dir = await openWorkspaceDialog();
    if (!dir) return;
    await this.openWorkspacePath(dir);
  }

  private handleRemoveProject(projectId: string): void {
    const project = this.projectState.projects.find((p) => p.id === projectId);
    if (!project) return;

    const view = this.workspaceViews.get(projectId);
    if (view) {
      const conversationIds = view.getConversationIds();
      for (const cid of conversationIds) {
        const tab = view.getTabManager().getTabByConversationId(cid);
        if (tab) {
          view.getTabManager().closeTab(tab.id);
        } else {
          closeConversation(cid).catch((err) =>
            console.error(
              "Failed to close conversation on project removal:",
              err,
            ),
          );
        }
      }
      view.destroy();
      view.root.remove();
      this.workspaceViews.delete(projectId);
    }

    this.projectState.removeProject(projectId);

    const active = this.projectState.getActiveProject();
    if (active) {
      this.switchToWorkspace(active.id);
      return;
    }

    this.switchToHome();
  }

  private async createWorkbench(parentProjectId: string): Promise<void> {
    const parent = this.projectState.projects.find(
      (p) => p.id === parentProjectId,
    );
    if (!parent) return;

    const branchName = await promptForText({
      title: "New Workbench",
      description: `Create a git worktree branch from "${parent.name}"`,
      placeholder: "Branch name (e.g. feature-login)",
      confirmLabel: "Create",
      validate: (value) => {
        const trimmed = value.trim();
        if (!trimmed) return "Branch name is required";
        if (/\s/.test(trimmed)) return "Branch name cannot contain spaces";
        if (/[~^:?*[\\]/.test(trimmed))
          return "Branch name contains invalid characters";
        return null;
      },
    });
    if (branchName === null) return;
    const branch = branchName.trim();

    const parentPath = parent.workspacePath;
    const worktreePath = `${parentPath}--${branch}`;

    try {
      await gitWorktreeAdd(parentPath, worktreePath, branch);
    } catch (err) {
      this.notifications.error(
        `Failed to create workbench: ${err instanceof Error ? err.message : String(err)}`,
      );
      return;
    }

    const project = this.projectState.addWorkbench(
      parentProjectId,
      worktreePath,
      branch,
      "git-worktree",
    );
    this.switchToWorkspace(project.id);
  }

  private handleCreateWorkbench(payload: CreateWorkbenchEventPayload): void {
    const parent = this.projectState.projects.find(
      (p) => p.workspacePath === payload.parent_workspace_path,
    );
    if (!parent) {
      this.notifications.error(
        `No project found for workspace path: ${payload.parent_workspace_path}`,
      );
      return;
    }
    this.projectState.addWorkbench(
      parent.id,
      payload.worktree_path,
      payload.branch,
      "git-worktree",
    );
  }

  private async handleDeleteWorkbench(
    payload: DeleteWorkbenchEventPayload,
  ): Promise<void> {
    const project = this.projectState.projects.find(
      (p) => p.workspacePath === payload.workspace_path,
    );
    if (!project || !project.parentProjectId) return;
    await this.removeWorkbench(project.id);
  }

  private async removeWorkbench(projectId: string): Promise<void> {
    const project = this.projectState.projects.find((p) => p.id === projectId);
    if (!project || !project.parentProjectId) return;

    const parent = this.projectState.projects.find(
      (p) => p.id === project.parentProjectId,
    );
    if (!parent) return;

    // Close workspace view
    const view = this.workspaceViews.get(projectId);
    if (view) {
      const conversationIds = view.getConversationIds();
      for (const cid of conversationIds) {
        const tab = view.getTabManager().getTabByConversationId(cid);
        if (tab) {
          view.getTabManager().closeTab(tab.id);
        } else {
          closeConversation(cid).catch((err) =>
            console.error(
              "Failed to close conversation on workbench removal:",
              err,
            ),
          );
        }
      }
      view.destroy();
      view.root.remove();
      this.workspaceViews.delete(projectId);
    }

    // Remove the git worktree
    try {
      await gitWorktreeRemove(parent.workspacePath, project.workspacePath);
    } catch (err) {
      this.notifications.error(
        `Failed to remove workbench: ${err instanceof Error ? err.message : String(err)}`,
      );
    }

    this.projectState.removeProject(projectId);

    const active = this.projectState.getActiveProject();
    if (active) {
      this.switchToWorkspace(active.id);
      return;
    }
    this.switchToHome();
  }

  private async showManageRootsDialog(projectId: string): Promise<void> {
    const project = this.projectState.projects.find((p) => p.id === projectId);
    if (!project) return;

    const overlay = document.createElement("div");
    overlay.className = "text-prompt-overlay";
    overlay.dataset.testid = "manage-roots-dialog";

    const card = document.createElement("div");
    card.className = "text-prompt-card manage-roots-dialog";
    card.setAttribute("role", "dialog");
    card.setAttribute("aria-modal", "true");
    card.setAttribute("aria-label", "Manage Sub-Roots");

    const title = document.createElement("h3");
    title.className = "text-prompt-title";
    title.textContent = `Sub-Roots — ${project.name}`;
    card.appendChild(title);

    const desc = document.createElement("p");
    desc.className = "text-prompt-description";
    desc.textContent =
      "Sub-roots define workspace boundaries for Tycode agents within this project directory.";
    card.appendChild(desc);

    const listContainer = document.createElement("div");
    listContainer.className = "manage-roots-list";
    card.appendChild(listContainer);

    const syncView = () => {
      const view = this.workspaceViews.get(projectId);
      if (view) view.setRoots(project.roots);
    };

    const renderList = () => {
      listContainer.innerHTML = "";
      if (project.roots.length === 0) {
        const empty = document.createElement("div");
        empty.className = "manage-roots-empty";
        empty.textContent =
          "No sub-roots. The project path is the sole workspace root.";
        listContainer.appendChild(empty);
      } else {
        for (const root of project.roots) {
          const row = document.createElement("div");
          row.className = "manage-roots-row";

          const label = document.createElement("span");
          label.className = "manage-roots-label";
          label.textContent = root;
          row.appendChild(label);

          const removeBtn = document.createElement("button");
          removeBtn.className = "manage-roots-remove";
          removeBtn.textContent = "Remove";
          removeBtn.addEventListener("click", () => {
            this.projectState.removeProjectRoot(projectId, root);
            syncView();
            renderList();
          });
          row.appendChild(removeBtn);
          listContainer.appendChild(row);
        }
      }
    };
    renderList();

    const actions = document.createElement("div");
    actions.className = "text-prompt-actions";

    const addBtn = document.createElement("button");
    addBtn.className = "text-prompt-btn";
    addBtn.textContent = "Add Sub-Root\u2026";
    addBtn.addEventListener("click", async () => {
      let selected: string | null;
      try {
        selected = await pickSubRootDialog(project.workspacePath);
      } catch (err) {
        this.notifications.error(
          err instanceof Error ? err.message : String(err),
        );
        return;
      }
      if (!selected) return;
      this.projectState.addProjectRoot(projectId, selected);
      syncView();
      renderList();
    });
    actions.appendChild(addBtn);

    const doneBtn = document.createElement("button");
    doneBtn.className = "text-prompt-btn text-prompt-btn-primary";
    doneBtn.textContent = "Done";
    doneBtn.addEventListener("click", () => overlay.remove());
    actions.appendChild(doneBtn);

    card.appendChild(actions);
    overlay.appendChild(card);

    overlay.addEventListener("click", (e) => {
      if (e.target === overlay) overlay.remove();
    });
    card.addEventListener("keydown", (e) => {
      if (e.key === "Escape") overlay.remove();
    });

    document.body.appendChild(overlay);
  }

  private wireDockButtons(): void {
    document.getElementById("left-dock-btn")!.addEventListener("click", () => {
      this.getActiveView()?.getLayout().toggleLeftPanel();
    });
    document
      .getElementById("bottom-dock-btn")!
      .addEventListener("click", () => {
        this.getActiveView()?.getLayout().toggleBottomPanel();
      });
    document.getElementById("right-dock-btn")!.addEventListener("click", () => {
      this.getActiveView()?.getLayout().toggleRightPanel();
    });

    document.getElementById("left-dock-btn")!.title = "Toggle left dock";
    document.getElementById("bottom-dock-btn")!.title = "Toggle bottom dock";
    document.getElementById("right-dock-btn")!.title =
      `Toggle right dock (${formatShortcut("Ctrl+B")})`;
    document.getElementById("open-workspace-btn")!.title = "Open Workspace";
    document.getElementById("open-remote-workspace-btn")!.title =
      "Open Remote Workspace";
  }

  private openSettings(tab?: string): void {
    if (!this.settingsTabViewEl.classList.contains("hidden")) {
      if (tab) {
        this.settingsPanel.openToTab(tab);
        return;
      }
      this.closeSettings();
      return;
    }
    this.settingsPanel.refreshMcpHttpServerSettings();
    this.settingsPanel.refreshDriverMcpHttpServerSettings();
    this.settingsPanel.refreshHosts();
    this.settingsPanel.notifySelectedHostChanged();
    if (tab) {
      this.settingsPanel.openToTab(tab);
    }
    this.settingsTabViewEl.classList.remove("hidden");
    this.escapeStack.push("settings", () => this.closeSettings());
  }

  private closeSettings(): void {
    this.settingsTabViewEl.classList.add("hidden");
    this.escapeStack.remove("settings");
  }

  private registerCommands(): void {
    const cp = this.commandPalette;

    cp.registerCommand({
      id: "switch-git",
      label: "Switch to Git panel",
      shortcut: formatShortcut("Ctrl+2"),
      execute: () => {
        this.getActiveView()?.getLayout().switchTab("git");
      },
    });
    cp.registerCommand({
      id: "switch-files",
      label: "Switch to Files panel",
      shortcut: formatShortcut("Ctrl+3"),
      execute: () => {
        this.getActiveView()?.getLayout().switchTab("files");
      },
    });
    cp.registerCommand({
      id: "switch-diff",
      label: "Switch to Diff panel",
      shortcut: formatShortcut("Ctrl+4"),
      execute: () => {
        const view = this.getActiveView();
        if (!view) return;
        const fileTab = view.getTabManager().getPreferredFileTab();
        if (fileTab) {
          view.getTabManager().switchTo(fileTab.id);
        } else {
          view.getLayout().switchTab("diff");
        }
      },
    });
    cp.registerCommand({
      id: "toggle-right-panel",
      label: "Toggle right dock",
      shortcut: formatShortcut("Ctrl+B"),
      execute: () => this.getActiveView()?.getLayout().toggleRightPanel(),
    });
    cp.registerCommand({
      id: "open-settings",
      label: "Open Settings",
      shortcut: formatShortcut("Ctrl+,"),
      execute: () => {
        this.openSettings();
      },
    });
    cp.registerCommand({
      id: "open-sessions",
      label: "Open Sessions",
      execute: () => {
        void this.getActiveView()?.requestSessionsList();
      },
    });
    cp.registerCommand({
      id: "clear-chat",
      label: "Clear Chat",
      shortcut: formatShortcut("Ctrl+L"),
      execute: () => this.getActiveView()?.getChatPanel().clearChat(),
    });
    cp.registerCommand({
      id: "cancel-operation",
      label: "Cancel Operation",
      shortcut: "Escape",
      execute: () => {
        const cid = this.getActiveView()?.getActiveConversationId();
        if (cid !== null && cid !== undefined) cancelConversation(cid);
      },
    });
    cp.registerCommand({
      id: "refresh-git",
      label: "Refresh Git Status",
      shortcut: formatShortcut("Ctrl+Shift+R"),
      execute: () => this.getActiveView()?.getGitPanel().refresh(true),
    });
    cp.registerCommand({
      id: "toggle-fullscreen-chat",
      label: "Toggle Full-Screen Chat",
      shortcut: formatShortcut("Ctrl+Shift+F"),
      execute: () => this.getActiveView()?.getLayout().toggleFullScreenChat(),
    });
    cp.registerCommand({
      id: "focus-chat",
      label: "Focus Chat Input",
      shortcut: formatShortcut("Ctrl+1"),
      execute: () => {
        void this.getActiveView()?.focusChatTabOrCreate();
      },
    });
    cp.registerCommand({
      id: "keyboard-shortcuts",
      label: "Keyboard Shortcuts",
      shortcut: formatShortcut("Ctrl+/"),
      execute: () => showCheatSheet(),
    });
    cp.registerCommand({
      id: "open-workspace",
      label: "Open Workspace",
      execute: () => this.openWorkspace(),
    });
    cp.registerCommand({
      id: "open-remote-workspace",
      label: "Open Remote Workspace",
      execute: () => this.openRemoteWorkspace(),
    });
    cp.registerCommand({
      id: "toggle-theme",
      label: "Toggle Theme",
      execute: () => {
        const root = document.documentElement;
        const isCurrentlyLight = root.dataset.theme === "light";
        const newTheme = isCurrentlyLight ? "dark" : "light";
        root.dataset.theme = newTheme === "light" ? "light" : "";
        localStorage.setItem("tyde-theme", newTheme);
      },
    });
    cp.registerCommand({
      id: "toggle-settings",
      label: "Toggle Settings",
      execute: () => {
        this.openSettings();
      },
    });
    cp.registerCommand({
      id: "toggle-sessions",
      label: "Toggle Sessions",
      execute: () => {
        void this.getActiveView()?.requestSessionsList();
      },
    });
    cp.registerCommand({
      id: "toggle-task-list",
      label: "Toggle Task List",
      shortcut: formatShortcut("Ctrl+J"),
      execute: () => {
        this.getActiveView()?.getChatPanel().toggleActiveTaskBar();
      },
    });
    cp.registerCommand({
      id: "switch-workspace",
      label: "Switch Workspace",
      execute: () => this.openWorkspace(),
    });
    cp.registerCommand({
      id: "new-conversation",
      label: "New Conversation",
      shortcut: formatShortcut("Ctrl+N"),
      execute: async () => {
        const view = this.getActiveView();
        if (!view) {
          await this.openWorkspace();
          return;
        }
        try {
          await view.createNewConversationTab();
        } catch (err) {
          console.error("Failed to create conversation:", err);
          this.notifications.error("Failed to create conversation");
        }
      },
    });
    cp.registerCommand({
      id: "close-tab",
      label: "Close Tab",
      shortcut: formatShortcut("Ctrl+W"),
      execute: () => {
        const view = this.getActiveView();
        if (!view) return;
        const active = view.getTabManager().getActiveTab();
        if (active) view.getTabManager().closeTab(active.id);
      },
    });
    cp.registerCommand({
      id: "close-all-tabs",
      label: "Close All Tabs",
      execute: () => {
        const view = this.getActiveView();
        if (!view) return;
        view.getTabManager().closeAll();
        view.showEmptyState();
      },
    });
    cp.registerCommand({
      id: "send-feedback",
      label: "Send Feedback",
      execute: () => showFeedbackDialog(),
    });
  }

  private registerWorkflowCommands(view: WorkspaceView): void {
    for (const id of this.registeredWorkflowCommandIds) {
      this.commandPalette.unregisterCommand(id);
    }
    this.registeredWorkflowCommandIds = [];

    const store = view.getWorkflowStore();
    for (const workflow of store.getAll()) {
      const commandId = `workflow:${workflow.id}`;
      this.commandPalette.registerCommand({
        id: commandId,
        label: `Run Workflow: ${workflow.name}`,
        execute: () => {
          view.getWorkflowsPanel().runWorkflow(workflow);
          view.getLayout().showWidget("workflows");
        },
      });
      this.registeredWorkflowCommandIds.push(commandId);
    }

    const builderId = "workflow:new";
    this.commandPalette.registerCommand({
      id: builderId,
      label: "New Workflow",
      execute: () => {
        const activeView = this.getActiveView();
        if (activeView) {
          (activeView as WorkspaceView).getWorkflowsPanel().onNewWorkflow?.();
        }
      },
    });
    this.registeredWorkflowCommandIds.push(builderId);
  }

  private registerKeyboardShortcuts(): void {
    const kb = this.keyboard;

    kb.register("Ctrl+K", () => this.commandPalette.toggle());
    kb.register("Ctrl+P", () => this.commandPalette.toggle());
    kb.register("Ctrl+N", async () => {
      const view = this.getActiveView();
      if (!view) {
        await this.openWorkspace();
        return;
      }
      try {
        await view.createNewConversationTab();
      } catch (err) {
        console.error("Failed to create conversation:", err);
        this.notifications.error("Failed to create conversation");
      }
    });
    kb.register("Ctrl+,", () => {
      this.openSettings();
    });
    kb.register("Ctrl+L", () =>
      this.getActiveView()?.getChatPanel().clearChat(),
    );
    kb.register("Ctrl+B", () =>
      this.getActiveView()?.getLayout().toggleRightPanel(),
    );
    kb.register("Ctrl+J", () =>
      this.getActiveView()?.getChatPanel().toggleActiveTaskBar(),
    );
    kb.register("Ctrl+/", () => showCheatSheet());
    kb.register("Ctrl+Shift+F", () =>
      this.getActiveView()?.getLayout().toggleFullScreenChat(),
    );
    kb.register("Ctrl+Shift+R", () =>
      this.getActiveView()?.getGitPanel().refresh(true),
    );
    kb.register("Ctrl+1", () => {
      void this.getActiveView()?.focusChatTabOrCreate();
    });
    kb.register("Ctrl+2", () => {
      this.getActiveView()?.getLayout().switchTab("git");
    });
    kb.register("Ctrl+3", () => {
      this.getActiveView()?.getLayout().switchTab("files");
    });
    kb.register("Ctrl+4", () => {
      const view = this.getActiveView();
      if (!view) return;
      const fileTab = view.getTabManager().getPreferredFileTab();
      if (fileTab) {
        view.getTabManager().switchTo(fileTab.id);
      } else {
        view.getLayout().switchTab("diff");
      }
    });
    kb.register("Ctrl+5", () => {
      this.openSettings();
    });
    kb.register("Ctrl+W", () => {
      const view = this.getActiveView();
      if (!view) return;
      const active = view.getTabManager().getActiveTab();
      if (active) view.getTabManager().closeTab(active.id);
    });
    kb.register("Ctrl+Tab", () => {
      const view = this.getActiveView();
      if (!view) return;
      const tabs = view.getTabManager().getTabs();
      const active = view.getTabManager().getActiveTab();
      if (tabs.length < 2 || !active) return;
      const idx = tabs.findIndex((t) => t.id === active.id);
      view.getTabManager().switchTo(tabs[(idx + 1) % tabs.length].id);
    });
    kb.register("Ctrl+Shift+Tab", () => {
      const view = this.getActiveView();
      if (!view) return;
      const tabs = view.getTabManager().getTabs();
      const active = view.getTabManager().getActiveTab();
      if (tabs.length < 2 || !active) return;
      const idx = tabs.findIndex((t) => t.id === active.id);
      view
        .getTabManager()
        .switchTo(tabs[(idx - 1 + tabs.length) % tabs.length].id);
    });
    kb.register("Ctrl+=", () => adjustFontSize(1));
    kb.register("Ctrl+-", () => adjustFontSize(-1));
    kb.register("Escape", () => {
      const view = this.getActiveView();
      if (!view) return;
      if (view.getChatPanel().isTyping()) {
        const cid = view.getActiveConversationId();
        if (cid !== null) cancelConversation(cid);
        return;
      }
      view.getChatPanel().focusInput();
    });

    document.addEventListener("keydown", (e: KeyboardEvent) => {
      if (!(e.metaKey || e.ctrlKey) || e.shiftKey || e.altKey) return;
      const key = e.key.toLowerCase();
      if (key !== "f" && key !== "g") return;
      if (this.isEditableTarget(e.target)) return;

      const view = this.getActiveView();
      if (!view) return;

      const handled =
        key === "f"
          ? view.focusFindInActiveFileViewer()
          : view.focusGoToLineInActiveFileViewer();
      if (!handled) return;

      e.preventDefault();
      e.stopPropagation();
    });

    kb.enable();
  }

  private isEditableTarget(target: EventTarget | null): boolean {
    if (!(target instanceof HTMLElement)) return false;
    if (target.closest('[data-keyboard-shortcuts="off"]')) return true;
    if (target.isContentEditable) return true;
    return (
      target.closest(
        'input, textarea, [contenteditable="true"], [contenteditable=""]',
      ) !== null
    );
  }

  private async bootstrapStartup(): Promise<void> {
    const initialWorkspace = await getInitialWorkspace();
    if (initialWorkspace) {
      await this.openWorkspacePath(initialWorkspace);
      const view = this.activeWorkspaceId
        ? this.workspaceViews.get(this.activeWorkspaceId)
        : null;
      if (view) await view.whenReady();
      return;
    }

    const startupProject = this.projectState.getActiveProject();

    if (!startupProject) {
      this.switchToHome();
      return;
    }

    const view = this.getOrCreateWorkspaceView(
      startupProject.id,
      startupProject.workspacePath,
      startupProject.name,
    );
    this.switchToWorkspace(startupProject.id);

    try {
      await view.ensureAdminSubprocess();
      this.settingsPanel.adminId = view.getAdminId();
    } catch (err) {
      console.error("Failed to spawn admin subprocess:", err);
    }

    await view.whenReady();
  }
}
