import type { RuntimeAgentStatus } from "./bridge";

export type AgentCardAction = "interrupt" | "terminate" | "remove";

export interface AgentInfo {
  agentId?: number;
  conversationId: number;
  name: string;
  summary: string;
  isTyping: boolean;
  hasError?: boolean;
  createdAt: number;
  projectId: string;
  runtimeStatus?: RuntimeAgentStatus;
  parentAgentId?: number | null;
}

export class AgentsPanel {
  private container: HTMLElement;
  private agents: AgentInfo[] = [];
  private projectFilter: string | null = null;
  private collapsedParents: Set<number> = new Set();
  public onAgentClick: ((agent: AgentInfo) => void) | null = null;
  public onAgentAction:
    | ((agent: AgentInfo, action: AgentCardAction) => void)
    | null = null;
  public onChange: ((agents: AgentInfo[]) => void) | null = null;

  constructor(container: HTMLElement) {
    this.container = container;
    this.container.classList.add("agents-panel");
    this.render();
  }

  addAgent(info: AgentInfo): void {
    this.upsertAgent(info);
  }

  upsertAgent(info: AgentInfo): void {
    const idx = this.agents.findIndex(
      (a) => a.conversationId === info.conversationId,
    );
    if (idx === -1) {
      this.agents.push(info);
    } else {
      const next = { ...this.agents[idx], ...info };
      if (this.sameAgentInfo(this.agents[idx], next)) {
        return;
      }
      this.agents[idx] = next;
    }
    this.render();
    this.notifyChanged();
  }

  updateAgent(conversationId: number, updates: Partial<AgentInfo>): void {
    const agent = this.agents.find((a) => a.conversationId === conversationId);
    if (!agent) return;
    Object.assign(agent, updates);
    this.render();
    this.notifyChanged();
  }

  removeAgent(conversationId: number): void {
    this.agents = this.agents.filter(
      (a) => a.conversationId !== conversationId,
    );
    this.render();
    this.notifyChanged();
  }

  clear(): void {
    this.agents = [];
    this.render();
    this.notifyChanged();
  }

  getAgents(): AgentInfo[] {
    return this.agents;
  }

  getAgentByConversationId(id: number): AgentInfo | undefined {
    return this.agents.find((a) => a.conversationId === id);
  }

  setProjectFilter(projectId: string | null): void {
    this.projectFilter = projectId;
    this.render();
  }

  render(): void {
    this.container.innerHTML = "";

    const header = document.createElement("div");
    header.className = "agents-panel-header";
    header.innerHTML = '<span class="agents-panel-title">Agents</span>';
    this.container.appendChild(header);

    const filtered = this.filteredAgents();
    if (filtered.length === 0) {
      this.container.appendChild(this.buildEmptyState());
      return;
    }

    // Build parent→children map keyed by agentId
    const childrenByParent = new Map<number, AgentInfo[]>();
    const roots: AgentInfo[] = [];

    for (const agent of filtered) {
      if (
        agent.parentAgentId != null &&
        filtered.some((a) => a.agentId === agent.parentAgentId)
      ) {
        const siblings = childrenByParent.get(agent.parentAgentId) ?? [];
        siblings.push(agent);
        childrenByParent.set(agent.parentAgentId, siblings);
      } else {
        roots.push(agent);
      }
    }

    roots.sort((a, b) => b.createdAt - a.createdAt);
    for (const children of childrenByParent.values()) {
      children.sort((a, b) => b.createdAt - a.createdAt);
    }

    const list = document.createElement("div");
    list.className = "agents-list";

    for (const root of roots) {
      const children =
        root.agentId != null ? (childrenByParent.get(root.agentId) ?? []) : [];
      list.appendChild(this.buildCard(root, children.length));

      if (children.length > 0) {
        const collapsed =
          root.agentId != null && this.collapsedParents.has(root.agentId);
        if (!collapsed) {
          for (const child of children) {
            const grandchildren =
              child.agentId != null
                ? (childrenByParent.get(child.agentId) ?? [])
                : [];
            list.appendChild(this.buildCard(child, grandchildren.length, true));
          }
        }
      }
    }

    this.container.appendChild(list);
  }

  private filteredAgents(): AgentInfo[] {
    if (this.projectFilter === null) return [...this.agents];
    return this.agents.filter((a) => a.projectId === this.projectFilter);
  }

  private buildEmptyState(): HTMLElement {
    const el = document.createElement("div");
    el.className = "agents-empty-state";
    el.innerHTML =
      '<div class="agents-empty-icon">🤖</div>' +
      '<div class="agents-empty-label">No agents yet</div>' +
      '<div class="agents-empty-hint">Conversations will appear here when created</div>';
    return el;
  }

  private buildCard(
    agent: AgentInfo,
    childCount: number = 0,
    isChild: boolean = false,
  ): HTMLElement {
    const card = document.createElement("div");
    const statusClass = agent.isTyping
      ? "running"
      : agent.hasError
        ? "error"
        : "completed";
    card.className = `agent-card agent-card-${statusClass}`;
    if (isChild) card.classList.add("agent-card-child");
    card.dataset.testid = "agent-card";

    const header = document.createElement("div");
    header.className = "agent-card-header";

    const titleRow = document.createElement("div");
    titleRow.className = "agent-card-title-row";

    if (childCount > 0 && agent.agentId != null) {
      const toggle = document.createElement("button");
      toggle.type = "button";
      toggle.className = "agent-card-collapse-toggle";
      toggle.dataset.testid = "agent-card-collapse";
      const collapsed = this.collapsedParents.has(agent.agentId);
      toggle.textContent = collapsed ? "▶" : "▼";
      toggle.title = collapsed ? "Expand sub-agents" : "Collapse sub-agents";
      toggle.setAttribute("aria-label", toggle.title);
      toggle.addEventListener("click", (event) => {
        event.stopPropagation();
        if (agent.agentId == null) return;
        if (this.collapsedParents.has(agent.agentId)) {
          this.collapsedParents.delete(agent.agentId);
        } else {
          this.collapsedParents.add(agent.agentId);
        }
        this.render();
      });
      titleRow.appendChild(toggle);
    }

    const title = document.createElement("span");
    title.className = "agent-card-title";
    title.textContent = agent.name;
    titleRow.appendChild(title);

    header.appendChild(titleRow);

    const headerRight = document.createElement("div");
    headerRight.className = "agent-card-header-right";

    if (childCount > 0) {
      const badge = document.createElement("span");
      badge.className = "agent-card-child-badge";
      badge.dataset.testid = "agent-card-child-badge";
      badge.textContent = `${childCount} sub-agent${childCount === 1 ? "" : "s"}`;
      headerRight.appendChild(badge);
    }

    const statusEl = this.buildStatusIndicator(agent);
    if (statusEl) headerRight.appendChild(statusEl);
    header.appendChild(headerRight);

    const summary = document.createElement("div");
    summary.className = "agent-card-summary";
    summary.textContent = agent.summary;

    const footer = document.createElement("div");
    footer.className = "agent-card-footer";

    const time = document.createElement("div");
    time.className = "agent-card-time";
    time.textContent = this.formatRelativeTime(agent.createdAt);

    footer.appendChild(time);

    const actions = this.buildActionRow(agent);
    if (actions) {
      footer.appendChild(actions);
    }

    card.appendChild(header);
    card.appendChild(summary);
    card.appendChild(footer);

    card.addEventListener("click", () => {
      if (this.onAgentClick) this.onAgentClick(agent);
    });

    return card;
  }

  private buildStatusIndicator(agent: AgentInfo): HTMLElement | null {
    if (agent.runtimeStatus === "waiting_input") {
      const icon = document.createElement("span");
      icon.className = "agent-status-icon";
      icon.textContent = "⏸";
      return icon;
    }
    if (agent.isTyping) {
      const spinner = document.createElement("div");
      spinner.className = "loading-spinner";
      return spinner;
    }
    return null;
  }

  private buildActionRow(agent: AgentInfo): HTMLElement | null {
    const row = document.createElement("div");
    row.className = "agent-card-actions";

    const isRuntime = agent.runtimeStatus != null && Boolean(agent.agentId);
    if (isRuntime) {
      if (this.canInterrupt(agent)) {
        row.appendChild(this.buildActionButton(agent, "interrupt"));
      }
      if (this.canTerminate(agent)) {
        row.appendChild(this.buildActionButton(agent, "terminate"));
      }
      if (this.canRemove(agent)) {
        row.appendChild(this.buildActionButton(agent, "remove"));
      }
    } else if (agent.isTyping) {
      row.appendChild(this.buildActionButton(agent, "interrupt"));
    } else {
      row.appendChild(this.buildActionButton(agent, "remove"));
    }

    return row.childElementCount > 0 ? row : null;
  }

  private buildActionButton(
    agent: AgentInfo,
    action: AgentCardAction,
  ): HTMLButtonElement {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "agent-card-action-btn";
    btn.dataset.testid = `agent-card-${action}`;
    btn.textContent = this.actionIcon(action);
    const tooltip = this.actionTooltip(agent, action);
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

  private actionTooltip(agent: AgentInfo, action: AgentCardAction): string {
    if (action === "interrupt") {
      return agent.runtimeStatus != null
        ? "Interrupt this agent run"
        : "Interrupt this conversation";
    }
    if (action === "terminate") return "Terminate this agent";
    return agent.runtimeStatus != null
      ? "Remove this agent card"
      : "Close and remove this conversation";
  }

  private canInterrupt(agent: AgentInfo): boolean {
    return (
      agent.runtimeStatus === "queued" ||
      agent.runtimeStatus === "running" ||
      agent.runtimeStatus === "waiting_input"
    );
  }

  private canTerminate(agent: AgentInfo): boolean {
    return this.canInterrupt(agent);
  }

  private canRemove(agent: AgentInfo): boolean {
    return (
      agent.runtimeStatus === "completed" ||
      agent.runtimeStatus === "failed" ||
      agent.runtimeStatus === "cancelled"
    );
  }

  private formatRelativeTime(epochMs: number): string {
    const deltaMs = Date.now() - epochMs;
    if (deltaMs < 60_000) return "just now";

    const minutes = Math.floor(deltaMs / 60_000);
    if (minutes < 60) return `${minutes}m ago`;

    const hours = Math.floor(minutes / 60);
    return `${hours}h ago`;
  }

  private notifyChanged(): void {
    this.onChange?.(this.agents.map((agent) => ({ ...agent })));
  }

  private sameAgentInfo(a: AgentInfo, b: AgentInfo): boolean {
    return (
      a.agentId === b.agentId &&
      a.conversationId === b.conversationId &&
      a.name === b.name &&
      a.summary === b.summary &&
      a.isTyping === b.isTyping &&
      a.hasError === b.hasError &&
      a.createdAt === b.createdAt &&
      a.projectId === b.projectId &&
      a.runtimeStatus === b.runtimeStatus &&
      a.parentAgentId === b.parentAgentId
    );
  }
}
