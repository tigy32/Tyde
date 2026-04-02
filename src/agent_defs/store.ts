import {
  deleteAgentDefinition,
  listAgentDefinitions,
  saveAgentDefinition,
} from "../bridge";
import type { AgentDefinitionEntry } from "./types";

export class AgentDefinitionStore {
  private definitions: AgentDefinitionEntry[] = [];
  private readonly workspacePath: string;
  private readonly listeners = new Set<() => void>();
  private inFlightLoad: Promise<void> | null = null;
  private loaded = false;

  constructor(workspacePath: string) {
    this.workspacePath = workspacePath.trim();
  }

  get contextWorkspacePath(): string {
    return this.workspacePath;
  }

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  private notifyChange(): void {
    for (const listener of this.listeners) {
      listener();
    }
  }

  async load(force = false): Promise<void> {
    if (!force && this.inFlightLoad) {
      await this.inFlightLoad;
      return;
    }
    if (!force && this.loaded) return;

    const run = (async () => {
      this.definitions = await listAgentDefinitions(
        this.workspacePath || undefined,
      );
      this.loaded = true;
      this.notifyChange();
    })();

    this.inFlightLoad = run;
    try {
      await run;
    } finally {
      if (this.inFlightLoad === run) {
        this.inFlightLoad = null;
      }
    }
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
    await this.load(true);
  }

  async delete(id: string): Promise<void> {
    const entry = this.getById(id);
    if (!entry) return;
    await deleteAgentDefinition(
      id,
      entry.scope,
      this.workspacePath || undefined,
    );
    await this.load(true);
  }
}
