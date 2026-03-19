import { deleteWorkflow, listWorkflows, saveWorkflow } from "../bridge";
import type { WorkflowDefinition } from "./types";

export class WorkflowStore {
  private workflows: WorkflowDefinition[] = [];
  private scopes = new Map<string, "global" | "project">();
  private workspacePath: string;
  onChange: (() => void) | null = null;

  constructor(workspacePath: string) {
    this.workspacePath = workspacePath;
  }

  async load(): Promise<void> {
    const entries = await listWorkflows(this.workspacePath || undefined);
    this.workflows = entries.map((e) => ({
      id: e.id,
      name: e.name,
      description: e.description,
      trigger: e.trigger,
      steps: e.steps,
    }));
    this.scopes.clear();
    for (const entry of entries) {
      this.scopes.set(entry.id, entry.scope as "global" | "project");
    }
    this.onChange?.();
  }

  getAll(): WorkflowDefinition[] {
    return this.workflows;
  }

  getByTrigger(trigger: string): WorkflowDefinition | undefined {
    return this.workflows.find((w) => w.trigger === trigger);
  }

  getById(id: string): WorkflowDefinition | undefined {
    return this.workflows.find((w) => w.id === id);
  }

  getScope(id: string): "global" | "project" {
    return this.scopes.get(id) ?? "global";
  }

  async save(
    workflow: WorkflowDefinition,
    scope: "global" | "project",
  ): Promise<void> {
    const json = JSON.stringify(workflow);
    await saveWorkflow(json, scope, this.workspacePath || undefined);
    await this.load();
  }

  async delete(id: string): Promise<void> {
    const scope = this.scopes.get(id) ?? "global";
    await deleteWorkflow(id, scope, this.workspacePath || undefined);
    await this.load();
  }
}
