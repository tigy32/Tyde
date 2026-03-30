import {
  deleteAgentDefinition,
  listAgentDefinitions,
  saveAgentDefinition,
} from "../bridge";
import type { AgentDefinitionEntry } from "./types";

export class AgentDefinitionStore {
  private definitions: AgentDefinitionEntry[] = [];
  private workspacePath: string;
  onChange: (() => void) | null = null;

  constructor(workspacePath: string) {
    this.workspacePath = workspacePath;
  }

  async load(): Promise<void> {
    this.definitions = await listAgentDefinitions(
      this.workspacePath || undefined,
    );
    this.onChange?.();
  }

  getAll(): AgentDefinitionEntry[] {
    return this.definitions;
  }

  getById(id: string): AgentDefinitionEntry | undefined {
    return this.definitions.find((d) => d.id === id);
  }

  async save(
    definition: AgentDefinitionEntry,
    scope: "global" | "project",
  ): Promise<void> {
    const json = JSON.stringify(definition);
    await saveAgentDefinition(json, scope, this.workspacePath || undefined);
    await this.load();
  }

  async delete(id: string): Promise<void> {
    const entry = this.getById(id);
    if (!entry) return;
    await deleteAgentDefinition(
      id,
      entry.scope,
      this.workspacePath || undefined,
    );
    await this.load();
  }
}
