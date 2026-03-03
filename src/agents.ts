import type { RuntimeAgentStatus } from "./bridge";

export type AgentCardAction = "interrupt" | "terminate" | "remove";

export interface AgentInfo {
  agentId?: number;
  conversationId: number;
  name: string;
  kind?: "conversation" | "runtime";
  status: "running" | "completed" | "error";
  summary: string;
  isTyping?: boolean;
  createdAt: number;
  projectId: string;
  keepAliveWithoutTab?: boolean;
  runtimeStatus?: RuntimeAgentStatus;
}

export class AgentsPanel {
  private container: HTMLElement;
  private agents: AgentInfo[] = [];
  private projectFilter: string | null = null;
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

    filtered.sort((a, b) => b.createdAt - a.createdAt);

    const list = document.createElement("div");
    list.className = "agents-list";
    for (const agent of filtered) {
      list.appendChild(this.buildCard(agent));
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

  private buildCard(agent: AgentInfo): HTMLElement {
    const card = document.createElement("div");
    card.className = `agent-card agent-card-${agent.status}`;
    card.dataset.testid = "agent-card";

    const header = document.createElement("div");
    header.className = "agent-card-header";

    const title = document.createElement("span");
    title.className = "agent-card-title";
    title.textContent = agent.name;
    header.appendChild(title);
    header.appendChild(this.buildStatusIndicator(agent));

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

  private buildStatusIndicator(agent: AgentInfo): HTMLElement {
    if (agent.runtimeStatus === "waiting_input") {
      const icon = document.createElement("span");
      icon.className = "agent-status-icon";
      icon.textContent = "⏸";
      return icon;
    }
    if (agent.status === "running") {
      const spinner = document.createElement("div");
      spinner.className = "loading-spinner";
      return spinner;
    }
    const icon = document.createElement("span");
    icon.className = "agent-status-icon";
    icon.textContent = agent.status === "completed" ? "✓" : "✗";
    return icon;
  }

  private buildActionRow(agent: AgentInfo): HTMLElement | null {
    const row = document.createElement("div");
    row.className = "agent-card-actions";

    const isRuntime = agent.kind === "runtime" && Boolean(agent.agentId);
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
    } else if (agent.status === "running") {
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
      return agent.kind === "runtime"
        ? "Interrupt this agent run"
        : "Interrupt this conversation";
    }
    if (action === "terminate") return "Terminate this agent";
    return agent.kind === "runtime"
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
      a.kind === b.kind &&
      a.name === b.name &&
      a.status === b.status &&
      a.summary === b.summary &&
      a.isTyping === b.isTyping &&
      a.createdAt === b.createdAt &&
      a.projectId === b.projectId &&
      a.keepAliveWithoutTab === b.keepAliveWithoutTab &&
      a.runtimeStatus === b.runtimeStatus
    );
  }
}
