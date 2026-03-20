import type { AgentCardAction } from "./agents";
import type { BackendDepResult, BackendKind, RuntimeAgent } from "./bridge";
import {
  checkBackendDependencies as checkBackendDependenciesBridge,
  installBackendDependency as installBackendDependencyBridge,
} from "./bridge";
import { formatShortcut } from "./keyboard";
import type { Project, ProjectStateManager } from "./project_state";
import {
  getCachedDependencyStatus,
  getEnabledBackendPreferences,
  getEnabledBackends,
  isOnboardingComplete,
  markOnboardingComplete,
  setEnabledBackendPreferences,
  syncDisabledBackendsToRust,
} from "./settings";

const STATUS_COLORS: Record<string, string> = {
  active: "#4CAF50",
  needs_attention: "#FF9800",
  idle: "#607D8B",
};

type HomeTab = "projects" | "agents";

export class HomeView {
  private container: HTMLElement;
  private projectState: ProjectStateManager;
  private bridgeChatEnabled = false;
  private bridgeChatLabel = "Bridge";
  private bridgeChatDisabledReason =
    "Enable Loopback MCP Control in Settings to start Bridge chats.";
  private activeTab: HomeTab = "projects";
  private cachedAgents: RuntimeAgent[] | null = null;
  private agentsLoading = false;
  private actionListenerController: AbortController | null = null;
  private bridgeMenuOpen = false;
  private collapsedParents: Set<number> = new Set();
  private homeHideInactive = false;
  private homeHideSubAgents = false;
  private homeHideOtherWorkspaces = false;
  private homeSearchQuery = "";
  private wizardStep: 0 | 1 | 2 = 0;
  private wizardDependencyStatus: Record<BackendKind, BackendDepResult> | null =
    null;
  private wizardInstallingBackends: Set<BackendKind> = new Set();
  private wizardInstallError: Map<BackendKind, string> = new Map();
  onOpenWorkspace: (() => void) | null = null;
  onOpenRemoteWorkspace: (() => void) | null = null;
  onNewBridgeChat: ((backendOverride?: BackendKind) => void) | null = null;
  onSwitchProject: ((id: string) => void) | null = null;
  resolveProjectAgentCounts:
    | ((projectId: string) => { total: number; active: number })
    | null = null;
  resolveAllAgents: (() => Promise<RuntimeAgent[]>) | null = null;
  onAgentClick: ((agent: RuntimeAgent) => void) | null = null;
  onAgentAction:
    | ((agent: RuntimeAgent, action: AgentCardAction) => void)
    | null = null;

  constructor(container: HTMLElement, projectState: ProjectStateManager) {
    this.container = container;
    this.projectState = projectState;
  }

  show(): void {
    this.container.style.display = "";
    this.render();
  }
  hide(): void {
    this.container.style.display = "none";
  }

  setBridgeChatAvailability(
    enabled: boolean,
    reason?: string | null,
    label?: string,
  ): void {
    this.bridgeChatEnabled = enabled;
    if (typeof reason === "string" && reason.trim().length > 0) {
      this.bridgeChatDisabledReason = reason;
    }
    if (typeof label === "string" && label.trim().length > 0) {
      this.bridgeChatLabel = label.trim();
    }
    this.render();
  }

  render(): void {
    this.container.innerHTML = "";

    const wrapper = document.createElement("div");
    wrapper.className = "home-view";
    wrapper.dataset.testid = "home-view";

    if (!isOnboardingComplete()) {
      wrapper.appendChild(this.buildWizard());
      this.container.appendChild(wrapper);
      return;
    }

    wrapper.appendChild(this.buildHeader());
    wrapper.appendChild(this.buildActions());
    wrapper.appendChild(this.buildKeyboardHints());
    wrapper.appendChild(this.buildTabBar());

    if (this.activeTab === "projects") {
      if (this.projectState.projects.length > 0) {
        wrapper.appendChild(this.buildProjectGrid());
      } else {
        wrapper.appendChild(this.buildEmptyProjectsState());
      }
    } else {
      wrapper.appendChild(this.buildAgentsSection());
      this.loadAgents();
    }

    this.container.appendChild(wrapper);
  }

  private buildTabBar(): HTMLElement {
    const bar = document.createElement("div");
    bar.className = "home-tab-bar";
    bar.dataset.testid = "home-tab-bar";

    const projectsTab = document.createElement("button");
    projectsTab.className = `home-tab${this.activeTab === "projects" ? " home-tab-active" : ""}`;
    projectsTab.dataset.testid = "home-tab-projects";
    projectsTab.textContent = "Projects";
    projectsTab.addEventListener("click", () => {
      if (this.activeTab === "projects") return;
      this.activeTab = "projects";
      this.render();
    });

    const agentsTab = document.createElement("button");
    agentsTab.className = `home-tab${this.activeTab === "agents" ? " home-tab-active" : ""}`;
    agentsTab.dataset.testid = "home-tab-agents";
    agentsTab.textContent = "Agents";
    agentsTab.addEventListener("click", () => {
      if (this.activeTab === "agents") return;
      this.activeTab = "agents";
      // Force a fresh fetch when switching to the agents tab
      this.cachedAgents = null;
      this.render();
    });

    bar.appendChild(projectsTab);
    bar.appendChild(agentsTab);
    return bar;
  }

  private buildAgentsSection(): HTMLElement {
    const section = document.createElement("div");
    section.className = "home-agents-section";
    section.dataset.testid = "home-agents-section";

    section.appendChild(this.buildAgentsToolbar());

    if (this.agentsLoading && !this.cachedAgents) {
      const loading = document.createElement("div");
      loading.className = "panel-loading";
      loading.innerHTML =
        '<div class="loading-spinner"></div> Loading agents\u2026';
      section.appendChild(loading);
      return section;
    }

    const filtered = this.filteredHomeAgents();
    if (filtered.length === 0) {
      const empty = document.createElement("div");
      empty.className = "agents-empty-state";
      empty.innerHTML =
        '<div class="agents-empty-icon">🤖</div>' +
        '<div class="agents-empty-label">No agents running</div>' +
        '<div class="agents-empty-hint">Agents from all workspaces and MCP will appear here</div>';
      section.appendChild(empty);
      return section;
    }

    // Build parent→children map for hierarchy display
    const childrenByParent = new Map<number, RuntimeAgent[]>();
    const roots: RuntimeAgent[] = [];
    for (const agent of filtered) {
      if (
        agent.parent_agent_id != null &&
        filtered.some((a) => a.agent_id === agent.parent_agent_id)
      ) {
        const siblings = childrenByParent.get(agent.parent_agent_id) ?? [];
        siblings.push(agent);
        childrenByParent.set(agent.parent_agent_id, siblings);
      } else {
        roots.push(agent);
      }
    }

    roots.sort((a, b) => b.created_at_ms - a.created_at_ms);

    const list = document.createElement("div");
    list.className = "home-agents-list";

    for (const agent of roots) {
      const children = childrenByParent.get(agent.agent_id) ?? [];
      list.appendChild(this.buildAgentCard(agent, children.length, false));
      if (children.length > 0 && !this.collapsedParents.has(agent.agent_id)) {
        children.sort((a, b) => a.created_at_ms - b.created_at_ms);
        for (const child of children) {
          list.appendChild(this.buildAgentCard(child, 0, true));
        }
      }
    }

    section.appendChild(list);
    return section;
  }

  private buildAgentsToolbar(): HTMLElement {
    const toolbar = document.createElement("div");
    toolbar.className = "agents-toolbar";
    toolbar.dataset.testid = "home-agents-toolbar";

    const searchInput = document.createElement("input");
    searchInput.type = "text";
    searchInput.className = "agents-search";
    searchInput.placeholder = "Search agents\u2026";
    searchInput.setAttribute("aria-label", "Search agents");
    searchInput.value = this.homeSearchQuery;
    searchInput.addEventListener("input", () => {
      this.homeSearchQuery = searchInput.value;
      this.render();
    });

    const hideInactiveBtn = document.createElement("button");
    hideInactiveBtn.type = "button";
    hideInactiveBtn.className = "agents-toolbar-btn";
    if (this.homeHideInactive)
      hideInactiveBtn.classList.add("agents-toolbar-btn-active");
    hideInactiveBtn.dataset.testid = "home-agents-hide-inactive";
    hideInactiveBtn.textContent = "◑";
    hideInactiveBtn.title = "Hide inactive agents";
    hideInactiveBtn.setAttribute("aria-label", "Hide inactive agents");
    hideInactiveBtn.addEventListener("click", () => {
      this.homeHideInactive = !this.homeHideInactive;
      this.render();
    });

    const hideSubBtn = document.createElement("button");
    hideSubBtn.type = "button";
    hideSubBtn.className = "agents-toolbar-btn";
    if (this.homeHideSubAgents)
      hideSubBtn.classList.add("agents-toolbar-btn-active");
    hideSubBtn.dataset.testid = "home-agents-hide-subagents";
    hideSubBtn.textContent = "⊟";
    hideSubBtn.title = "Hide sub-agents";
    hideSubBtn.setAttribute("aria-label", "Hide sub-agents");
    hideSubBtn.addEventListener("click", () => {
      this.homeHideSubAgents = !this.homeHideSubAgents;
      this.render();
    });

    const hideOtherBtn = document.createElement("button");
    hideOtherBtn.type = "button";
    hideOtherBtn.className = "agents-toolbar-btn";
    if (this.homeHideOtherWorkspaces)
      hideOtherBtn.classList.add("agents-toolbar-btn-active");
    hideOtherBtn.dataset.testid = "home-agents-hide-other-workspaces";
    hideOtherBtn.textContent = "⌂";
    hideOtherBtn.title = "Hide agents from other workspaces";
    hideOtherBtn.setAttribute(
      "aria-label",
      "Hide agents from other workspaces",
    );
    hideOtherBtn.addEventListener("click", () => {
      this.homeHideOtherWorkspaces = !this.homeHideOtherWorkspaces;
      this.render();
    });

    toolbar.appendChild(searchInput);
    toolbar.appendChild(hideInactiveBtn);
    toolbar.appendChild(hideSubBtn);
    toolbar.appendChild(hideOtherBtn);
    return toolbar;
  }

  private filteredHomeAgents(): RuntimeAgent[] {
    const agents = this.cachedAgents ?? [];
    let result = [...agents];
    if (this.homeHideInactive) {
      result = result.filter((a) => a.is_running);
    }
    if (this.homeHideSubAgents) {
      result = result.filter((a) => a.parent_agent_id == null);
    }
    if (this.homeHideOtherWorkspaces) {
      result = result.filter(
        (a) => a.parent_agent_id != null || a.workspace_roots.length === 0,
      );
    }
    if (this.homeSearchQuery) {
      const q = this.homeSearchQuery.toLowerCase();
      result = result.filter(
        (a) =>
          a.name.toLowerCase().includes(q) ||
          a.summary.toLowerCase().includes(q),
      );
    }
    return result;
  }

  private buildAgentCard(
    agent: RuntimeAgent,
    childCount: number,
    isChild: boolean,
  ): HTMLElement {
    const card = document.createElement("div");
    card.className = `agent-card agent-card-${this.agentStatusClass(agent)}`;
    if (isChild) card.classList.add("agent-card-child");
    card.dataset.testid = "home-agent-card";

    const header = document.createElement("div");
    header.className = "agent-card-header";

    const titleRow = document.createElement("div");
    titleRow.className = "agent-card-title-row";

    if (childCount > 0) {
      const toggle = document.createElement("button");
      toggle.type = "button";
      toggle.className = "agent-card-collapse-toggle";
      toggle.dataset.testid = "agent-card-collapse";
      const collapsed = this.collapsedParents.has(agent.agent_id);
      toggle.textContent = collapsed ? "▶" : "▼";
      toggle.title = collapsed ? "Expand sub-agents" : "Collapse sub-agents";
      toggle.setAttribute("aria-label", toggle.title);
      toggle.addEventListener("click", (event) => {
        event.stopPropagation();
        if (this.collapsedParents.has(agent.agent_id)) {
          this.collapsedParents.delete(agent.agent_id);
        } else {
          this.collapsedParents.add(agent.agent_id);
        }
        this.render();
      });
      titleRow.appendChild(toggle);
    }

    const title = document.createElement("span");
    title.className = "agent-card-title";
    title.textContent = agent.name || `Agent #${agent.agent_id}`;
    titleRow.appendChild(title);
    header.appendChild(titleRow);

    const headerRight = document.createElement("div");
    headerRight.className = "agent-card-header-right";

    if (agent.agent_type) {
      const typeBadge = document.createElement("span");
      typeBadge.className = "agent-card-type-badge";
      typeBadge.dataset.testid = "agent-card-type-badge";
      typeBadge.textContent = agent.agent_type;
      headerRight.appendChild(typeBadge);
    }

    if (childCount > 0) {
      const badge = document.createElement("span");
      badge.className = "agent-card-child-badge";
      badge.dataset.testid = "agent-card-child-badge";
      badge.textContent = `${childCount} sub-agent${childCount === 1 ? "" : "s"}`;
      headerRight.appendChild(badge);
    }

    if (agent.is_running) {
      const statusEl = document.createElement("div");
      statusEl.className = "loading-spinner";
      headerRight.appendChild(statusEl);
    }
    header.appendChild(headerRight);
    card.appendChild(header);

    if (!isChild && agent.workspace_roots.length > 0) {
      const workspace = document.createElement("div");
      workspace.className = "home-agent-workspace";
      const root = agent.workspace_roots[0];
      workspace.textContent = root.split("/").pop() || root;
      card.appendChild(workspace);
    }

    if (agent.summary) {
      const summary = document.createElement("div");
      summary.className = "agent-card-summary";
      summary.textContent = agent.summary;
      card.appendChild(summary);
    }

    const time = document.createElement("div");
    time.className = "agent-card-time";
    time.textContent = this.formatRelativeTime(agent.created_at_ms);

    const footer = document.createElement("div");
    footer.className = "agent-card-footer";
    footer.appendChild(time);

    const actions = this.buildActionRow(agent);
    if (actions) {
      footer.appendChild(actions);
    }

    card.appendChild(footer);

    card.addEventListener("click", () => this.onAgentClick?.(agent));
    return card;
  }

  setAgents(agents: RuntimeAgent[]): void {
    if (this.sameRuntimeAgents(this.cachedAgents, agents)) {
      this.agentsLoading = false;
      return;
    }
    this.cachedAgents = [...agents];
    this.agentsLoading = false;
    if (this.activeTab === "agents") {
      this.render();
    }
  }

  private agentStatusClass(agent: RuntimeAgent): string {
    if (agent.is_running) return "running";
    if (agent.last_error != null) return "error";
    return "completed";
  }

  private formatRelativeTime(epochMs: number): string {
    const deltaMs = Date.now() - epochMs;
    if (deltaMs < 60_000) return "just now";
    const minutes = Math.floor(deltaMs / 60_000);
    if (minutes < 60) return `${minutes}m ago`;
    const hours = Math.floor(minutes / 60);
    return `${hours}h ago`;
  }

  private loadAgents(): void {
    if (
      !this.resolveAllAgents ||
      this.agentsLoading ||
      this.cachedAgents !== null
    )
      return;
    this.agentsLoading = true;

    this.resolveAllAgents()
      .then((agents) => {
        this.cachedAgents = agents;
        this.agentsLoading = false;
        if (this.activeTab === "agents") this.render();
      })
      .catch((err) => {
        console.error("Failed to load agents:", err);
        this.agentsLoading = false;
        this.cachedAgents = [];
        if (this.activeTab === "agents") this.render();
      });
  }

  private buildActionRow(agent: RuntimeAgent): HTMLElement | null {
    const row = document.createElement("div");
    row.className = "agent-card-actions";

    if (this.canInterrupt(agent)) {
      row.appendChild(this.buildActionButton(agent, "interrupt"));
    }
    if (this.canTerminate(agent)) {
      row.appendChild(this.buildActionButton(agent, "terminate"));
    }
    if (this.canRemove(agent)) {
      row.appendChild(this.buildActionButton(agent, "remove"));
    }

    return row.childElementCount > 0 ? row : null;
  }

  private buildActionButton(
    agent: RuntimeAgent,
    action: AgentCardAction,
  ): HTMLButtonElement {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "agent-card-action-btn";
    btn.dataset.testid = `agent-card-${action}`;
    btn.textContent = this.actionIcon(action);
    const tooltip = this.actionTooltip(action);
    btn.title = tooltip;
    btn.setAttribute("aria-label", tooltip);
    btn.addEventListener("click", (event) => {
      event.stopPropagation();
      this.onAgentAction?.(agent, action);
    });
    return btn;
  }

  private actionIcon(action: AgentCardAction): string {
    if (action === "interrupt") return "⏸";
    if (action === "terminate") return "⏹";
    return "✕";
  }

  private actionTooltip(action: AgentCardAction): string {
    if (action === "interrupt") return "Interrupt this agent run";
    if (action === "terminate") return "Terminate this agent";
    return "Remove this agent card";
  }

  private canInterrupt(agent: RuntimeAgent): boolean {
    return agent.is_running;
  }

  private canTerminate(agent: RuntimeAgent): boolean {
    return this.canInterrupt(agent);
  }

  private canRemove(agent: RuntimeAgent): boolean {
    return !agent.is_running;
  }

  private sameRuntimeAgents(
    current: RuntimeAgent[] | null,
    next: RuntimeAgent[],
  ): boolean {
    if (!current || current.length !== next.length) return false;
    return current.every((agent, index) => {
      const candidate = next[index];
      return (
        agent.agent_id === candidate.agent_id &&
        agent.name === candidate.name &&
        agent.agent_type === candidate.agent_type &&
        agent.is_running === candidate.is_running &&
        agent.summary === candidate.summary &&
        agent.updated_at_ms === candidate.updated_at_ms &&
        agent.ended_at_ms === candidate.ended_at_ms
      );
    });
  }

  private buildHeader(): HTMLElement {
    const header = document.createElement("div");
    header.className = "home-header";

    const logo = document.createElement("div");
    logo.className = "home-logo";
    const img = document.createElement("img");
    img.src = "tycode-tiger.png";
    img.alt = "Tyde";
    img.className = "home-logo-img";
    logo.appendChild(img);

    const title = document.createElement("h1");
    title.className = "home-title";
    title.textContent = "Tyde";

    const subtitle = document.createElement("p");
    subtitle.className = "home-subtitle";
    subtitle.textContent = "Coding Agent Studio";

    header.appendChild(logo);
    header.appendChild(title);
    header.appendChild(subtitle);
    return header;
  }

  private buildActions(): HTMLElement {
    this.actionListenerController?.abort();
    this.actionListenerController = new AbortController();
    const signal = this.actionListenerController.signal;

    const actions = document.createElement("div");
    actions.className = "home-actions";

    // -- Bridge split button (main + dropdown) --
    const splitContainer = document.createElement("div");
    splitContainer.className = "home-bridge-split";

    const bridgeBtn = document.createElement("button");
    bridgeBtn.className =
      "home-action-btn home-action-primary home-bridge-split-main";
    bridgeBtn.dataset.testid = "home-new-bridge-chat";
    bridgeBtn.textContent = `New ${this.bridgeChatLabel} Chat`;
    bridgeBtn.disabled = !this.bridgeChatEnabled;
    if (!this.bridgeChatEnabled) {
      bridgeBtn.title = this.bridgeChatDisabledReason;
    } else {
      bridgeBtn.title = `Start a new ${this.bridgeChatLabel} chat`;
    }
    bridgeBtn.addEventListener("click", () => {
      if (!this.bridgeChatEnabled) return;
      this.onNewBridgeChat?.();
    });

    const menuBtn = document.createElement("button");
    menuBtn.className =
      "home-action-btn home-action-primary home-bridge-split-menu";
    menuBtn.dataset.testid = "home-bridge-menu-btn";
    menuBtn.textContent = "\u25BE";
    menuBtn.disabled = !this.bridgeChatEnabled;
    menuBtn.title = `Choose backend for new ${this.bridgeChatLabel} chat`;
    menuBtn.setAttribute("aria-haspopup", "menu");
    menuBtn.setAttribute("aria-expanded", "false");

    const menu = document.createElement("div");
    menu.className = "home-bridge-menu";
    menu.hidden = true;
    menu.setAttribute("role", "menu");
    menu.dataset.testid = "home-bridge-menu";

    const allBackends: { kind: BackendKind; label: string }[] = [
      { kind: "tycode", label: "Tycode" },
      { kind: "codex", label: "Codex" },
      { kind: "claude", label: "Claude" },
      { kind: "kiro", label: "Kiro" },
    ];
    const enabledSet = new Set(getEnabledBackends());
    const backends = allBackends.filter((b) => enabledSet.has(b.kind));
    if (backends.length === 0) {
      const hint = document.createElement("div");
      hint.className = "home-bridge-menu-empty";
      hint.textContent =
        "No backends enabled. Enable at least one in Settings → Backends.";
      menu.appendChild(hint);
    }
    for (const { kind, label } of backends) {
      const item = document.createElement("button");
      item.className = "home-bridge-menu-item";
      item.setAttribute("role", "menuitem");
      item.dataset.testid = `home-bridge-${kind}`;
      item.textContent = `New ${label} ${this.bridgeChatLabel}`;
      item.addEventListener("click", () => {
        closeMenu();
        this.onNewBridgeChat?.(kind);
      });
      menu.appendChild(item);
    }

    const openMenu = (): void => {
      if (!this.bridgeChatEnabled || this.bridgeMenuOpen) return;
      this.bridgeMenuOpen = true;
      menu.hidden = false;
      menuBtn.setAttribute("aria-expanded", "true");
      splitContainer.classList.add("open");
    };

    const closeMenu = (): void => {
      this.bridgeMenuOpen = false;
      menu.hidden = true;
      menuBtn.setAttribute("aria-expanded", "false");
      splitContainer.classList.remove("open");
    };

    if (this.bridgeMenuOpen) {
      menu.hidden = false;
      menuBtn.setAttribute("aria-expanded", "true");
      splitContainer.classList.add("open");
    }

    menuBtn.addEventListener("click", (event) => {
      event.stopPropagation();
      if (!this.bridgeChatEnabled) return;
      if (menu.hidden) {
        openMenu();
      } else {
        closeMenu();
      }
    });

    const handleOutsidePointer = (event: PointerEvent): void => {
      const target = event.target as Node | null;
      if (!splitContainer.contains(target) && !menu.contains(target)) {
        closeMenu();
      }
    };
    document.addEventListener("pointerdown", handleOutsidePointer, { signal });

    const handleEscape = (event: KeyboardEvent): void => {
      if (event.key === "Escape") closeMenu();
    };
    window.addEventListener("keydown", handleEscape, { signal });

    splitContainer.appendChild(bridgeBtn);
    splitContainer.appendChild(menuBtn);
    splitContainer.appendChild(menu);

    // -- Other action buttons --
    const openBtn = document.createElement("button");
    openBtn.className = "home-action-btn home-action-secondary";
    openBtn.dataset.testid = "home-open-workspace";
    openBtn.textContent = "Open Workspace";
    openBtn.addEventListener("click", () => this.onOpenWorkspace?.());

    const remoteBtn = document.createElement("button");
    remoteBtn.className = "home-action-btn home-action-secondary";
    remoteBtn.dataset.testid = "home-open-remote";
    remoteBtn.textContent = "Open Remote";
    remoteBtn.addEventListener("click", () => this.onOpenRemoteWorkspace?.());

    actions.appendChild(splitContainer);
    actions.appendChild(openBtn);
    actions.appendChild(remoteBtn);
    return actions;
  }

  private buildProjectGrid(): HTMLElement {
    const section = document.createElement("div");
    section.className = "home-projects-section";

    const heading = document.createElement("h2");
    heading.className = "home-section-title";
    heading.textContent = "Open Projects";
    section.appendChild(heading);

    const grid = document.createElement("div");
    grid.className = "home-project-grid";

    for (const project of this.projectState.projects) {
      // Skip workbenches — they're rendered under their parent
      if (project.parentProjectId) continue;
      grid.appendChild(this.buildProjectCard(project));

      const workbenches = this.projectState.getWorkbenches(project.id);
      for (const wb of workbenches) {
        const wbCard = this.buildProjectCard(wb);
        wbCard.classList.add("home-workbench-card");
        grid.appendChild(wbCard);
      }
    }

    section.appendChild(grid);
    return section;
  }

  private buildProjectCard(project: Project): HTMLElement {
    const card = document.createElement("div");
    card.className = "home-project-card";
    card.dataset.testid = "project-card";
    card.addEventListener("click", () => this.onSwitchProject?.(project.id));

    const cardHeader = document.createElement("div");
    cardHeader.className = "home-card-header";

    const avatar = document.createElement("div");
    avatar.className = "home-card-avatar";
    avatar.textContent = project.parentProjectId
      ? "⑂"
      : project.name.charAt(0).toUpperCase();

    const nameCol = document.createElement("div");
    nameCol.className = "home-card-name-col";

    const name = document.createElement("div");
    name.className = "home-card-name";
    name.dataset.testid = "project-name";
    name.textContent = project.name;

    const path = document.createElement("div");
    path.className = "home-card-path";
    path.textContent = project.workspacePath;

    nameCol.appendChild(name);
    nameCol.appendChild(path);

    cardHeader.appendChild(avatar);
    cardHeader.appendChild(nameCol);
    card.appendChild(cardHeader);

    const meta = document.createElement("div");
    meta.className = "home-card-meta";

    const statusDot = document.createElement("span");
    statusDot.className = "home-card-status-dot";
    statusDot.style.background =
      STATUS_COLORS[project.status] ?? STATUS_COLORS.idle;

    const statusLabel = document.createElement("span");
    statusLabel.className = "home-card-status-label";
    statusLabel.textContent =
      project.status === "needs_attention" ? "needs attention" : project.status;

    const convCount = document.createElement("span");
    convCount.className = "home-card-conv-count";
    convCount.dataset.testid = "project-agent-count";
    const resolved = this.resolveProjectAgentCounts?.(project.id);
    const total = resolved?.total ?? project.conversationIds.length;
    const active = resolved?.active ?? total;
    convCount.textContent =
      total === active
        ? `${active} active agent${active !== 1 ? "s" : ""}`
        : `${active} active / ${total} total`;

    meta.appendChild(statusDot);
    meta.appendChild(statusLabel);
    meta.appendChild(convCount);
    card.appendChild(meta);

    return card;
  }

  // --- Setup Wizard ---

  private buildWizard(): HTMLElement {
    const wizard = document.createElement("div");
    wizard.className = "home-wizard";
    wizard.dataset.testid = "home-wizard";

    if (this.wizardStep === 0) {
      this.buildWizardWelcome(wizard);
    } else if (this.wizardStep === 1) {
      this.buildWizardBackends(wizard);
    } else {
      this.buildWizardDone(wizard);
    }

    return wizard;
  }

  private buildWizardWelcome(wizard: HTMLElement): void {
    const logo = document.createElement("div");
    logo.className = "home-logo";
    const img = document.createElement("img");
    img.src = "tycode-tiger.png";
    img.alt = "Tyde";
    img.className = "home-logo-img";
    logo.appendChild(img);
    wizard.appendChild(logo);

    const title = document.createElement("h1");
    title.className = "home-title";
    title.textContent = "Welcome to Tyde";
    wizard.appendChild(title);

    const desc = document.createElement("p");
    desc.className = "home-wizard-text";
    desc.textContent =
      "Tyde is a coding agent studio that connects to multiple AI backends. " +
      "Let\u2019s set up your first backend so you can start working.";
    wizard.appendChild(desc);

    const nextBtn = document.createElement("button");
    nextBtn.className = "home-action-btn home-action-primary";
    nextBtn.dataset.testid = "wizard-next";
    nextBtn.textContent = "Set Up Backends";
    nextBtn.addEventListener("click", () => {
      this.wizardStep = 1;
      this.wizardDependencyStatus = getCachedDependencyStatus();
      this.render();
    });
    wizard.appendChild(nextBtn);
  }

  private buildWizardBackends(wizard: HTMLElement): void {
    const title = document.createElement("h2");
    title.className = "home-wizard-step-title";
    title.textContent = "Configure Backends";
    wizard.appendChild(title);

    const desc = document.createElement("p");
    desc.className = "home-wizard-text";
    desc.textContent =
      "Enable the coding backends you have installed. " +
      "At least one backend is required.";
    wizard.appendChild(desc);

    const backends: {
      kind: BackendKind;
      label: string;
      binary: string;
      description: string;
    }[] = [
      {
        kind: "tycode",
        label: "Tycode",
        binary: "tycode-subprocess",
        description: "Built-in Tyde backend.",
      },
      {
        kind: "codex",
        label: "Codex",
        binary: "codex",
        description: "OpenAI Codex CLI backend.",
      },
      {
        kind: "claude",
        label: "Claude Code",
        binary: "claude",
        description: "Anthropic Claude Code CLI backend.",
      },
      {
        kind: "kiro",
        label: "Kiro",
        binary: "kiro-cli",
        description: "Kiro CLI backend.",
      },
    ];

    const list = document.createElement("div");
    list.className = "home-wizard-backend-list";

    const enabledPrefs = getEnabledBackendPreferences();

    for (const { kind, label, binary, description } of backends) {
      const dep = this.wizardDependencyStatus?.[kind];
      const depMissing = dep !== undefined && !dep.available;

      const card = document.createElement("div");
      card.className = "home-wizard-backend-card";
      card.dataset.testid = `wizard-backend-${kind}`;

      const row = document.createElement("div");
      row.className = "home-wizard-backend-row";

      const labelCol = document.createElement("div");
      labelCol.className = "home-wizard-backend-label-col";

      const nameEl = document.createElement("span");
      nameEl.className = "home-wizard-backend-name";
      nameEl.textContent = label;
      labelCol.appendChild(nameEl);

      const descEl = document.createElement("span");
      descEl.className = "home-wizard-backend-desc";
      descEl.textContent = description;
      labelCol.appendChild(descEl);

      row.appendChild(labelCol);

      const toggle = document.createElement("label");
      toggle.className = "settings-toggle";
      const input = document.createElement("input");
      input.type = "checkbox";
      input.checked = enabledPrefs.includes(kind) && !depMissing;
      input.disabled = depMissing;
      input.addEventListener("change", () => {
        const current = getEnabledBackendPreferences();
        if (input.checked) {
          if (!current.includes(kind)) current.push(kind);
        } else {
          const idx = current.indexOf(kind);
          if (idx !== -1) current.splice(idx, 1);
        }
        setEnabledBackendPreferences(current);
        syncDisabledBackendsToRust();
      });
      toggle.appendChild(input);
      const slider = document.createElement("span");
      slider.className = "settings-toggle-slider";
      toggle.appendChild(slider);
      row.appendChild(toggle);

      card.appendChild(row);

      if (depMissing) {
        const warning = document.createElement("p");
        warning.className = "home-wizard-backend-warning";
        warning.textContent = `"${binary}" was not found in PATH.`;
        card.appendChild(warning);

        const installing = this.wizardInstallingBackends.has(kind);
        const installError = this.wizardInstallError.get(kind);

        const installBtn = document.createElement("button");
        installBtn.className = "settings-install-btn";
        installBtn.dataset.testid = `wizard-install-${kind}`;
        installBtn.textContent = installing ? "Installing\u2026" : "Install";
        installBtn.disabled = installing;
        installBtn.addEventListener("click", () => {
          this.wizardInstallingBackends.add(kind);
          this.wizardInstallError.delete(kind);
          this.render();
          installBackendDependencyBridge(kind)
            .then(() => {
              this.wizardInstallingBackends.delete(kind);
              return checkBackendDependenciesBridge();
            })
            .then((status) => {
              this.wizardDependencyStatus = {
                tycode: status.tycode,
                codex: status.codex,
                claude: status.claude,
                kiro: status.kiro,
              };
              this.render();
            })
            .catch((err) => {
              this.wizardInstallingBackends.delete(kind);
              this.wizardInstallError.set(kind, String(err));
              this.render();
            });
        });
        card.appendChild(installBtn);

        if (installError) {
          const errorEl = document.createElement("p");
          errorEl.className = "home-wizard-backend-warning";
          errorEl.textContent = installError;
          card.appendChild(errorEl);
        }
      }

      list.appendChild(card);
    }

    wizard.appendChild(list);

    const btnRow = document.createElement("div");
    btnRow.className = "home-wizard-btn-row";

    const backBtn = document.createElement("button");
    backBtn.className = "home-action-btn home-action-secondary";
    backBtn.textContent = "Back";
    backBtn.addEventListener("click", () => {
      this.wizardStep = 0;
      this.render();
    });

    const nextBtn = document.createElement("button");
    nextBtn.className = "home-action-btn home-action-primary";
    nextBtn.dataset.testid = "wizard-next";
    nextBtn.textContent = "Continue";
    nextBtn.addEventListener("click", () => {
      this.wizardStep = 2;
      this.render();
    });

    btnRow.appendChild(backBtn);
    btnRow.appendChild(nextBtn);
    wizard.appendChild(btnRow);
  }

  private buildWizardDone(wizard: HTMLElement): void {
    const title = document.createElement("h2");
    title.className = "home-wizard-step-title";
    title.textContent = "You\u2019re All Set";
    wizard.appendChild(title);

    const enabledBackends = getEnabledBackends();
    const text = document.createElement("p");
    text.className = "home-wizard-text";
    text.textContent =
      enabledBackends.length > 0
        ? `${enabledBackends.length} backend${enabledBackends.length === 1 ? "" : "s"} enabled. You can change this anytime in Settings.`
        : "No backends are enabled yet. You can enable them in Settings later.";
    wizard.appendChild(text);

    wizard.appendChild(this.buildKeyboardHints());

    const btnRow = document.createElement("div");
    btnRow.className = "home-wizard-btn-row";

    const backBtn = document.createElement("button");
    backBtn.className = "home-action-btn home-action-secondary";
    backBtn.textContent = "Back";
    backBtn.addEventListener("click", () => {
      this.wizardStep = 1;
      this.render();
    });

    const getStartedBtn = document.createElement("button");
    getStartedBtn.className = "home-action-btn home-action-primary";
    getStartedBtn.dataset.testid = "wizard-finish";
    getStartedBtn.textContent = "Get Started";
    getStartedBtn.addEventListener("click", () => {
      markOnboardingComplete();
      this.render();
    });

    btnRow.appendChild(backBtn);
    btnRow.appendChild(getStartedBtn);
    wizard.appendChild(btnRow);
  }

  // --- Keyboard hints & empty state ---

  private buildKeyboardHints(): HTMLElement {
    const section = document.createElement("div");
    section.className = "home-keyboard-hints";
    section.dataset.testid = "home-keyboard-hints";

    const hints: [string, string][] = [
      [formatShortcut("Ctrl+K"), "Command Palette"],
      [formatShortcut("Ctrl+,"), "Settings"],
      [formatShortcut("Ctrl+/"), "Keyboard Shortcuts"],
      [formatShortcut("Ctrl+N"), "New Conversation"],
    ];

    for (const [shortcut, label] of hints) {
      const hint = document.createElement("div");
      hint.className = "home-keyboard-hint";

      const kbd = document.createElement("kbd");
      kbd.className = "home-kbd";
      kbd.textContent = shortcut;

      const descSpan = document.createElement("span");
      descSpan.className = "home-keyboard-hint-label";
      descSpan.textContent = label;

      hint.appendChild(kbd);
      hint.appendChild(descSpan);
      section.appendChild(hint);
    }

    return section;
  }

  private buildEmptyProjectsState(): HTMLElement {
    const empty = document.createElement("div");
    empty.className = "home-empty-projects";
    empty.dataset.testid = "home-empty-projects";

    const label = document.createElement("div");
    label.className = "home-empty-projects-label";
    label.textContent = "No open projects";

    const hint = document.createElement("div");
    hint.className = "home-empty-projects-hint";
    hint.textContent =
      "Open a workspace to get started, or start a Bridge chat above.";

    empty.appendChild(label);
    empty.appendChild(hint);
    return empty;
  }
}
