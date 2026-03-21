import { formatRelativeTime } from "../chat/message_renderer";
import { escapeHtml } from "../renderer";
import type { WorkflowEngine } from "./engine";
import type { WorkflowStore } from "./store";
import type {
  ActionRunState,
  WorkflowDefinition,
  WorkflowRunState,
} from "./types";

export class WorkflowsPanel {
  private container: HTMLElement;
  private store: WorkflowStore;
  private engine: WorkflowEngine;
  private hideCompleted = false;
  private expandedRunId: string | null = null;
  private runMenuOpen = false;
  private runMenuCloseHandler: ((e: MouseEvent) => void) | null = null;

  onEditWorkflow: ((workflow: WorkflowDefinition) => void) | null = null;
  onNewWorkflow: (() => void) | null = null;
  onManageWorkflows: (() => void) | null = null;
  onOpenAgentConversation:
    | ((conversationId: number, name: string) => void)
    | null = null;

  constructor(
    container: HTMLElement,
    store: WorkflowStore,
    engine: WorkflowEngine,
  ) {
    this.container = container;
    this.container.classList.add("workflows-panel");
    this.store = store;
    this.engine = engine;

    this.engine.onChange = (_run) => {
      this.render();
    };

    this.render();
  }

  render(): void {
    if (this.runMenuCloseHandler) {
      document.removeEventListener("mousedown", this.runMenuCloseHandler);
      this.runMenuCloseHandler = null;
    }
    this.container.innerHTML = "";
    this.container.appendChild(this.buildToolbar());

    const runs = this.filteredRuns();

    if (runs.length === 0) {
      this.container.appendChild(this.buildEmptyState());
      return;
    }

    const list = document.createElement("div");
    list.className = "workflows-list";

    for (const run of runs) {
      list.appendChild(this.buildRunCard(run));
      if (this.expandedRunId === run.runId) {
        list.appendChild(this.buildRunDetail(run));
      }
    }

    this.container.appendChild(list);
  }

  runWorkflow(workflow: WorkflowDefinition): void {
    const promise = this.engine.execute(workflow);
    const activeRuns = this.engine.getActiveRuns();
    const latestRun = activeRuns[activeRuns.length - 1];
    if (latestRun) {
      this.expandedRunId = latestRun.runId;
    }
    this.render();
    promise.then(() => this.render());
  }

  private filteredRuns(): WorkflowRunState[] {
    let runs = this.engine.getAllRuns();
    if (this.hideCompleted) {
      runs = runs.filter((r) => r.status === "running");
    }
    return runs.sort((a, b) => b.startedAt - a.startedAt);
  }

  private buildToolbar(): HTMLElement {
    const toolbar = document.createElement("div");
    toolbar.className = "workflows-toolbar";

    // Run button with dropdown
    const runWrap = document.createElement("div");
    runWrap.className = "workflows-run-wrap";

    const runBtn = document.createElement("button");
    runBtn.type = "button";
    runBtn.className = "workflows-run-btn";
    runBtn.textContent = "\u25B6 Run";
    runBtn.addEventListener("click", () => {
      this.runMenuOpen = !this.runMenuOpen;
      this.render();
    });
    runWrap.appendChild(runBtn);

    if (this.runMenuOpen) {
      const menu = this.buildRunMenu();
      runWrap.appendChild(menu);

      // Close on click-outside (deferred so this click doesn't immediately close)
      requestAnimationFrame(() => {
        const close = (e: MouseEvent) => {
          if (!runWrap.contains(e.target as Node)) {
            this.runMenuOpen = false;
            this.runMenuCloseHandler = null;
            document.removeEventListener("mousedown", close);
            this.render();
          }
        };
        this.runMenuCloseHandler = close;
        document.addEventListener("mousedown", close);
      });
    }

    toolbar.appendChild(runWrap);

    // Gear button — manage workflows
    const gearBtn = document.createElement("button");
    gearBtn.type = "button";
    gearBtn.className = "workflows-toolbar-btn";
    gearBtn.textContent = "\u2699";
    gearBtn.title = "Manage workflows";
    gearBtn.addEventListener("click", () => this.onManageWorkflows?.());
    toolbar.appendChild(gearBtn);

    // Hide completed toggle
    const hideBtn = document.createElement("button");
    hideBtn.type = "button";
    hideBtn.className = "workflows-toolbar-btn";
    if (this.hideCompleted)
      hideBtn.classList.add("workflows-toolbar-btn-active");
    hideBtn.textContent = "\u25D1";
    hideBtn.title = "Hide completed runs";
    hideBtn.addEventListener("click", () => {
      this.hideCompleted = !this.hideCompleted;
      this.render();
    });
    toolbar.appendChild(hideBtn);

    return toolbar;
  }

  private buildRunMenu(): HTMLElement {
    const menu = document.createElement("div");
    menu.className = "workflows-run-menu";

    const workflows = this.store.getAll();
    if (workflows.length === 0) {
      const empty = document.createElement("div");
      empty.className = "workflows-run-menu-empty";
      empty.textContent = "No workflows defined";
      menu.appendChild(empty);
      return menu;
    }

    for (const workflow of workflows) {
      const item = document.createElement("button");
      item.type = "button";
      item.className = "workflows-run-menu-item";
      item.textContent = workflow.name;
      item.addEventListener("click", () => {
        this.runMenuOpen = false;
        this.runWorkflow(workflow);
      });
      menu.appendChild(item);
    }

    return menu;
  }

  private buildEmptyState(): HTMLElement {
    const el = document.createElement("div");
    el.className = "workflows-empty-state";

    const icon = document.createElement("div");
    icon.className = "workflows-empty-icon";
    icon.textContent = "\u2699";
    el.appendChild(icon);

    const label = document.createElement("div");
    label.className = "workflows-empty-label";
    label.textContent = "No workflow runs yet";
    el.appendChild(label);

    const hint = document.createElement("div");
    hint.className = "workflows-empty-hint";
    hint.textContent = "Use \u25B6 Run to execute a workflow";
    el.appendChild(hint);

    return el;
  }

  private buildRunCard(run: WorkflowRunState): HTMLElement {
    const isRunning = run.status === "running";
    const failed = run.result != null && !run.result.success;

    const statusClass = isRunning ? "running" : failed ? "failed" : "completed";

    const card = document.createElement("div");
    card.className = `workflow-run-card workflow-run-card-${statusClass}`;
    card.addEventListener("click", () => {
      this.expandedRunId = this.expandedRunId === run.runId ? null : run.runId;
      this.render();
    });

    // Header: title + status indicator
    const header = document.createElement("div");
    header.className = "workflow-run-card-header";

    const titleRow = document.createElement("div");
    titleRow.className = "workflow-run-card-title-row";

    const title = document.createElement("span");
    title.className = "workflow-run-card-title";
    title.textContent = run.workflow.name;
    titleRow.appendChild(title);

    header.appendChild(titleRow);

    const headerRight = document.createElement("div");
    headerRight.className = "workflow-run-card-header-right";

    if (isRunning) {
      const spinner = document.createElement("div");
      spinner.className = "loading-spinner";
      headerRight.appendChild(spinner);
    }

    header.appendChild(headerRight);
    card.appendChild(header);

    // Summary: current step or result
    const summary = document.createElement("div");
    summary.className = "workflow-run-card-summary";
    summary.textContent = this.getRunSummary(run);
    card.appendChild(summary);

    // Footer: time + elapsed + actions
    const footer = document.createElement("div");
    footer.className = "workflow-run-card-footer";

    const time = document.createElement("div");
    time.className = "workflow-run-card-time";
    const relative = formatRelativeTime(run.startedAt);
    const elapsed = this.formatElapsed(run);
    time.textContent = `${relative} \u00B7 ${elapsed}`;
    footer.appendChild(time);

    const actions = document.createElement("div");
    actions.className = "workflow-run-card-actions";

    // Expand/collapse toggle
    const isExpanded = this.expandedRunId === run.runId;
    const expandBtn = document.createElement("button");
    expandBtn.type = "button";
    expandBtn.className = "workflow-run-card-action-btn";
    expandBtn.textContent = isExpanded ? "\u25BC" : "\u25B6";
    expandBtn.title = isExpanded ? "Collapse" : "Expand";
    expandBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      this.expandedRunId = this.expandedRunId === run.runId ? null : run.runId;
      this.render();
    });
    actions.appendChild(expandBtn);

    footer.appendChild(actions);
    card.appendChild(footer);

    return card;
  }

  /**
   * Flat layout: actions are rendered as top-level cards.
   * Steps with multiple actions are grouped with a subtle step label + left border.
   */
  private buildRunDetail(run: WorkflowRunState): HTMLElement {
    const detail = document.createElement("div");
    detail.className = "workflow-run-detail";

    for (const stepState of run.steps) {
      const hasMultipleActions = stepState.actions.length > 1;

      // For multi-action steps, wrap in a group with a step label
      if (hasMultipleActions) {
        const group = document.createElement("div");
        group.className = "workflow-step-group";

        const label = document.createElement("div");
        label.className = "workflow-step-group-label";
        label.textContent = stepState.step.name;
        group.appendChild(label);

        for (const actionState of stepState.actions) {
          group.appendChild(
            this.buildActionCard(
              actionState,
              run.workflow.name,
              stepState.step.name,
            ),
          );
        }

        detail.appendChild(group);
      } else if (stepState.actions.length === 1) {
        // Single action — render directly, no group wrapper
        detail.appendChild(
          this.buildActionCard(
            stepState.actions[0],
            run.workflow.name,
            stepState.step.name,
          ),
        );
      }
    }

    // Error block
    if (run.result && !run.result.success && run.result.error) {
      const errorEl = document.createElement("div");
      errorEl.className = "workflow-run-error";
      errorEl.innerHTML = `<strong>Error:</strong> ${escapeHtml(run.result.error)}`;
      detail.appendChild(errorEl);
    }

    return detail;
  }

  private buildActionCard(
    actionState: ActionRunState,
    workflowName: string,
    stepName: string,
  ): HTMLElement {
    const { action, status, result } = actionState;
    const isRunning = status === "running";
    const isPending = status === "pending";
    const failed = result != null && !result.success;

    const statusClass = isPending
      ? ""
      : isRunning
        ? "workflow-action-card-running"
        : failed
          ? "workflow-action-card-error"
          : result
            ? "workflow-action-card-completed"
            : "";

    const card = document.createElement("div");
    card.className = `workflow-action-card ${statusClass}`;

    // Agent cards are clickable to open conversation
    const convId = actionState.conversationId ?? result?.conversationId;
    if (action.type === "spawn_agent" && convId != null) {
      card.classList.add("workflow-action-card-clickable");
      const agentName = `${workflowName} - ${stepName}`;
      card.addEventListener("click", (e) => {
        e.stopPropagation();
        this.onOpenAgentConversation?.(convId, agentName);
      });
    }

    // Header
    const header = document.createElement("div");
    header.className = "workflow-action-card-header";

    const title = document.createElement("span");
    title.className = "workflow-action-card-title";
    if (action.type === "spawn_agent") {
      title.textContent = `${workflowName} - ${stepName}`;
    } else if (action.type === "run_command") {
      title.textContent = action.command;
      title.classList.add("workflow-action-card-title-mono");
    } else if (action.type === "run_workflow") {
      title.textContent = `Workflow: ${action.workflowId}`;
    }
    header.appendChild(title);

    const headerRight = document.createElement("div");
    headerRight.className = "workflow-action-card-header-right";

    if (action.type === "spawn_agent") {
      const badge = document.createElement("span");
      badge.className = "workflow-action-card-badge";
      badge.textContent = "Agent";
      headerRight.appendChild(badge);
    }

    if (isRunning) {
      const spinner = document.createElement("span");
      spinner.className = "loading-spinner";
      headerRight.appendChild(spinner);
    } else if (failed) {
      const indicator = document.createElement("span");
      indicator.className = "workflow-action-card-status-fail";
      indicator.textContent = "\u2717";
      headerRight.appendChild(indicator);
    } else if (result) {
      const indicator = document.createElement("span");
      indicator.className = "workflow-action-card-status-ok";
      indicator.textContent = "\u2713";
      headerRight.appendChild(indicator);
    }

    header.appendChild(headerRight);
    card.appendChild(header);

    // Summary line
    const summary = document.createElement("div");
    summary.className = "workflow-action-card-summary";
    if (action.type === "spawn_agent") {
      if (isRunning) {
        summary.textContent = "Running\u2026";
      } else if (isPending) {
        summary.textContent = "Waiting\u2026";
      } else if (failed) {
        summary.textContent = result.error ?? "Failed";
      } else if (result) {
        const outputPreview = result.output.split("\n")[0].slice(0, 100);
        summary.textContent = outputPreview || "Completed";
      } else {
        const prompt = action.prompt;
        summary.textContent =
          prompt.slice(0, 100) + (prompt.length > 100 ? "\u2026" : "");
      }
      card.appendChild(summary);
    } else if (result) {
      // Command/workflow: show output if completed
      const text = result.success
        ? result.output
        : (result.error ?? result.output);
      if (text) {
        const output = document.createElement("pre");
        output.className = "workflow-run-step-output";
        output.textContent = text.slice(0, 4000);
        if (text.length > 4000) output.textContent += "\n\u2026(truncated)";
        card.appendChild(output);
      }
    }

    return card;
  }

  private getRunSummary(run: WorkflowRunState): string {
    if (run.status === "running") {
      const currentStep = run.steps.find((s) => s.status === "running");
      if (currentStep) return `Running: ${currentStep.step.name}`;
      return "Starting\u2026";
    }
    if (run.result && !run.result.success) {
      const failedStep = run.steps.find((s) => s.result && !s.result.success);
      if (failedStep) return `Failed at: ${failedStep.step.name}`;
      return "Failed";
    }
    const count = run.steps.length;
    return `${count} step${count === 1 ? "" : "s"} completed`;
  }

  private formatElapsed(run: WorkflowRunState): string {
    const end = run.completedAt ?? Date.now();
    const ms = end - run.startedAt;
    if (ms < 1000) return `${ms}ms`;
    const seconds = Math.floor(ms / 1000);
    if (seconds < 60) return `${seconds}s`;
    const minutes = Math.floor(seconds / 60);
    const remainingSeconds = seconds % 60;
    return `${minutes}m ${remainingSeconds}s`;
  }
}
