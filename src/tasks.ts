import type { ContextBreakdown, TaskItem, TaskList } from "./types";

type SummaryView = "context" | "tasks";

const STATUS_ICONS: Record<TaskItem["status"], string> = {
  pending: "•",
  in_progress: "⟳",
  completed: "✓",
  failed: "✗",
};

interface SemanticCategory {
  label: string;
  percent: number;
  cssClass: string;
}

interface ContextMetrics {
  categories: SemanticCategory[];
  totalUsed: number;
  contextWindow: number;
  utilizationPct: number;
}

export class TaskPanel {
  private el: HTMLElement;
  private collapsed = false;
  private activeView: SummaryView = "context";
  private currentTaskList: TaskList | null = null;
  private contextBreakdown: ContextBreakdown | null = null;

  constructor(el: HTMLElement) {
    this.el = el;
    this.el.className = "task-list-panel hidden";
    this.el.dataset.testid = "task-panel";
    this.el.setAttribute("role", "region");
    this.el.setAttribute("aria-label", "Conversation summary");
    this.render();
  }

  update(taskList: TaskList): void {
    this.currentTaskList = taskList.tasks.length > 0 ? taskList : null;
    if (!this.currentTaskList && this.activeView === "tasks") {
      this.activeView = "context";
    }
    this.render();
  }

  setContextUsage(breakdown: ContextBreakdown): void {
    if (
      !Number.isFinite(breakdown.context_window) ||
      breakdown.context_window <= 0
    ) {
      console.warn(
        "[TaskPanel] setContextUsage rejected: invalid context_window",
        breakdown.context_window,
      );
      return;
    }
    if (!Number.isFinite(breakdown.input_tokens)) {
      console.warn(
        "[TaskPanel] setContextUsage rejected: invalid input_tokens",
        breakdown.input_tokens,
      );
      return;
    }
    this.contextBreakdown = breakdown;
    this.render();
  }

  clearContextUsage(): void {
    if (!this.contextBreakdown) return;
    this.contextBreakdown = null;
    this.render();
  }

  toggle(): void {
    if (!this.currentTaskList || this.currentTaskList.tasks.length === 0)
      return;
    this.activeView = this.activeView === "tasks" ? "context" : "tasks";
    this.render();
  }

  clearState(): void {
    this.currentTaskList = null;
    this.contextBreakdown = null;
    this.activeView = "context";
    this.render();
  }

  private render(): void {
    const hasContext = this.hasContextData();
    const hasTasks =
      !!this.currentTaskList && this.currentTaskList.tasks.length > 0;

    if (this.activeView === "tasks" && !hasTasks) {
      this.activeView = "context";
    }

    this.el.innerHTML = "";
    this.el.classList.remove("hidden");

    const wrapper = document.createElement("div");
    wrapper.className = "summary-panel";

    if (this.activeView === "tasks" && hasTasks && this.currentTaskList) {
      wrapper.appendChild(
        this.buildTaskView(
          this.currentTaskList,
          hasContext ? this.contextBreakdown : null,
        ),
      );
    } else if (hasContext && this.contextBreakdown) {
      wrapper.appendChild(
        this.buildContextView(
          this.contextBreakdown,
          hasTasks ? this.currentTaskList : null,
        ),
      );
    } else {
      wrapper.appendChild(
        this.buildEmptyContextView(hasTasks ? this.currentTaskList : null),
      );
    }

    this.el.appendChild(wrapper);
  }

  private buildTaskView(
    taskList: TaskList,
    contextBreakdown: ContextBreakdown | null,
  ): HTMLElement {
    const { tasks } = taskList;
    const container = document.createElement("div");
    container.className = "summary-task-view";

    if (tasks.length === 0) {
      return container;
    }

    const completedCount = tasks.filter((t) => t.status === "completed").length;
    const totalCount = tasks.length;
    const rows = this.collapsed ? this.pickCollapsedRows(tasks) : tasks;

    const header = document.createElement("button");
    header.type = "button";
    header.className = "task-list-header";
    header.setAttribute("aria-expanded", String(!this.collapsed));
    header.addEventListener("click", () => {
      this.collapsed = !this.collapsed;
      this.render();
    });

    const title = document.createElement("div");
    title.className = "task-list-title";

    const chevron = document.createElement("span");
    chevron.className = "task-list-chevron";
    chevron.textContent = this.collapsed ? "▶" : "▼";

    const heading = document.createElement("span");
    heading.className = "task-list-heading";
    heading.textContent = taskList.title || "Tasks";

    const progressText = document.createElement("span");
    progressText.className = "task-list-progress";
    progressText.textContent = `${completedCount}/${totalCount} tasks completed`;

    title.append(chevron, heading, progressText);
    header.appendChild(title);

    const items = document.createElement("div");
    items.className = "task-list-items";
    for (const task of rows) {
      items.appendChild(this.buildTaskRow(task));
    }

    container.append(header, items);

    if (contextBreakdown) {
      const miniBar = document.createElement("button");
      miniBar.type = "button";
      miniBar.className = "context-mini-bar";
      miniBar.setAttribute("aria-label", "View context usage");
      const metrics = this.computeContextMetrics(contextBreakdown);
      this.appendContextSegments(miniBar, metrics.categories);
      miniBar.addEventListener("click", () => {
        this.activeView = "context";
        this.render();
      });
      container.appendChild(miniBar);
    }

    return container;
  }

  private buildEmptyContextView(taskList: TaskList | null): HTMLElement {
    const container = document.createElement("div");
    container.className = "summary-context-view";

    const header = this.buildContextHeader();

    const bar = document.createElement("div");
    bar.className = "summary-context-bar";
    bar.dataset.testid = "context-bar";
    bar.setAttribute("role", "progressbar");
    bar.setAttribute("aria-label", "Context utilization");
    bar.setAttribute("aria-valuemin", "0");
    bar.setAttribute("aria-valuemax", "100");
    bar.setAttribute("aria-valuenow", "0");

    if (taskList && taskList.tasks.length > 0) {
      const meta = document.createElement("div");
      meta.className = "summary-context-meta";
      meta.append(this.buildTaskHint(taskList.tasks));
      container.append(header, bar, meta);
      return container;
    }

    container.append(header, bar);
    return container;
  }

  private buildContextView(
    bd: ContextBreakdown,
    taskList: TaskList | null,
  ): HTMLElement {
    const metrics = this.computeContextMetrics(bd);
    const hasDetailedBreakdown = this.hasDetailedBreakdown(bd);

    const container = document.createElement("div");
    container.className = "summary-context-view";
    container.appendChild(
      this.buildContextHeader(
        metrics.totalUsed,
        metrics.contextWindow,
        metrics.utilizationPct,
      ),
    );

    const bar = document.createElement("div");
    bar.className = "summary-context-bar";
    bar.dataset.testid = "context-bar";
    bar.setAttribute("role", "progressbar");
    bar.setAttribute("aria-label", "Context utilization");
    bar.setAttribute("aria-valuemin", "0");
    bar.setAttribute("aria-valuemax", "100");
    bar.setAttribute(
      "aria-valuenow",
      String(Math.round(metrics.utilizationPct)),
    );

    this.appendContextSegments(bar, metrics.categories);

    if (taskList && taskList.tasks.length > 0) {
      const meta = document.createElement("div");
      meta.className = "summary-context-meta";
      if (hasDetailedBreakdown) {
        meta.append(this.buildContextLegend());
      }
      meta.append(this.buildTaskHint(taskList.tasks));
      container.append(bar, meta);
      return container;
    }

    if (hasDetailedBreakdown) {
      container.append(bar, this.buildContextLegend());
      return container;
    }

    container.append(bar);
    return container;
  }

  private buildContextHeader(
    totalUsed?: number,
    contextWindow?: number,
    utilizationPct?: number,
  ): HTMLElement {
    const header = document.createElement("div");
    header.className = "summary-context-header";

    const title = document.createElement("span");
    title.className = "summary-context-title";
    title.textContent = "Context Usage";

    header.appendChild(title);

    if (
      Number.isFinite(totalUsed as number) &&
      Number.isFinite(contextWindow as number) &&
      Number.isFinite(utilizationPct as number)
    ) {
      const usage = document.createElement("span");
      usage.className = "summary-context-usage";
      usage.dataset.testid = "context-usage";
      usage.textContent = `${this.formatTokenCount(totalUsed as number)} / ${this.formatTokenCount(contextWindow as number)} tokens (${(utilizationPct as number).toFixed(1)}%)`;
      header.appendChild(usage);
    }

    return header;
  }

  private buildContextLegend(): HTMLElement {
    const breakdown = document.createElement("div");
    breakdown.className = "summary-context-breakdown";
    const labels: Array<[string, string]> = [
      ["System", "dot-system"],
      ["Tools", "dot-tools"],
      ["History", "dot-history"],
      ["Reasoning", "dot-reasoning"],
      ["Context", "dot-context"],
    ];
    for (const [label, dotClass] of labels) {
      breakdown.appendChild(this.buildBreakdownRow(label, dotClass));
    }
    return breakdown;
  }

  private appendContextSegments(
    target: HTMLElement,
    categories: SemanticCategory[],
  ): void {
    for (const cat of categories) {
      if (cat.percent <= 0) continue;
      const seg = document.createElement("span");
      seg.className = `summary-context-segment ${cat.cssClass}`;
      seg.dataset.testid = "context-segment";
      seg.style.width = `${cat.percent.toFixed(2)}%`;
      target.appendChild(seg);
    }
  }

  private buildBreakdownRow(label: string, dotClass: string): HTMLElement {
    const row = document.createElement("div");
    row.className = "context-breakdown-row";

    const left = document.createElement("span");
    left.className = "context-breakdown-label";

    const dot = document.createElement("span");
    dot.className = `context-breakdown-dot ${dotClass}`;
    left.append(dot, document.createTextNode(label));

    row.appendChild(left);
    return row;
  }

  private buildTaskHint(tasks: TaskItem[]): HTMLElement {
    const hint = document.createElement("button");
    hint.type = "button";
    hint.className = "context-task-hint";
    hint.textContent = this.buildTaskHintText(tasks);
    hint.addEventListener("click", () => {
      this.activeView = "tasks";
      this.render();
    });
    return hint;
  }

  private buildTaskHintText(tasks: TaskItem[]): string {
    const total = Math.max(1, tasks.length);
    const completed = tasks.filter(
      (task) => task.status === "completed",
    ).length;
    const hasInProgress = tasks.some((task) => task.status === "in_progress");
    if (hasInProgress) {
      const current = Math.min(completed + 1, total);
      return `Task ${current} of ${total} in progress \u2192`;
    }
    return `${completed}/${total} tasks done \u2192`;
  }

  private pickCollapsedRows(tasks: TaskItem[]): TaskItem[] {
    const inProgress = tasks.find((t) => t.status === "in_progress");
    if (inProgress) return [inProgress];
    const pending = tasks.find((t) => t.status === "pending");
    if (pending) return [pending];
    const failed = tasks.find((t) => t.status === "failed");
    if (failed) return [failed];
    if (tasks.every((t) => t.status === "completed")) {
      return [{ description: "All tasks completed!", status: "completed" }];
    }
    return tasks.slice(0, 1);
  }

  private buildTaskRow(task: TaskItem): HTMLElement {
    const row = document.createElement("div");
    row.className = `task-item-row status-${task.status}`;
    row.setAttribute("role", "listitem");
    row.setAttribute(
      "aria-label",
      `Task: ${task.description} - ${task.status}`,
    );

    const icon = document.createElement("span");
    icon.className = "task-item-icon";
    icon.textContent = STATUS_ICONS[task.status];

    const desc = document.createElement("span");
    desc.className = "task-item-desc";
    desc.textContent = task.description;

    row.append(icon, desc);
    return row;
  }

  private computeContextMetrics(bd: ContextBreakdown): ContextMetrics {
    const inputTokens = Math.max(0, bd.input_tokens);
    const systemBytes = Math.max(0, bd.system_prompt_bytes);
    const toolBytes = Math.max(0, bd.tool_io_bytes);
    const historyBytes = Math.max(0, bd.conversation_history_bytes);
    const reasoningBytes = Math.max(0, bd.reasoning_bytes);
    const contextBytes = Math.max(0, bd.context_injection_bytes);
    const totalBytes =
      systemBytes + toolBytes + historyBytes + reasoningBytes + contextBytes;

    const categories: SemanticCategory[] = [
      { label: "System", percent: 0, cssClass: "segment-system" },
      { label: "Tools", percent: 0, cssClass: "segment-tools" },
      { label: "History", percent: 0, cssClass: "segment-history" },
      { label: "Reasoning", percent: 0, cssClass: "segment-reasoning" },
      { label: "Context", percent: 0, cssClass: "segment-context" },
    ];

    const contextWindow = Math.max(1, bd.context_window);
    const utilizationPct = Math.min((inputTokens / contextWindow) * 100, 100);

    if (totalBytes <= 0) {
      categories[4].percent = utilizationPct;
      return {
        categories,
        totalUsed: inputTokens,
        contextWindow,
        utilizationPct,
      };
    }

    categories[0].percent = (systemBytes / totalBytes) * utilizationPct;
    categories[1].percent = (toolBytes / totalBytes) * utilizationPct;
    categories[2].percent = (historyBytes / totalBytes) * utilizationPct;
    categories[3].percent = (reasoningBytes / totalBytes) * utilizationPct;
    categories[4].percent = (contextBytes / totalBytes) * utilizationPct;

    return {
      categories,
      totalUsed: inputTokens,
      contextWindow,
      utilizationPct,
    };
  }

  private hasDetailedBreakdown(bd: ContextBreakdown): boolean {
    const totalBytes =
      Math.max(0, bd.system_prompt_bytes) +
      Math.max(0, bd.tool_io_bytes) +
      Math.max(0, bd.conversation_history_bytes) +
      Math.max(0, bd.reasoning_bytes) +
      Math.max(0, bd.context_injection_bytes);
    return totalBytes > 0;
  }

  private hasContextData(): boolean {
    if (!this.contextBreakdown) return false;
    return this.contextBreakdown.input_tokens > 0;
  }

  private formatTokenCount(tokens: number): string {
    const normalized = Math.max(0, tokens);
    if (normalized >= 1_000) {
      return `${(normalized / 1_000).toFixed(1)}K`;
    }
    return normalized.toLocaleString();
  }
}
