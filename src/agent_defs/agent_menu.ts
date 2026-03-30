import type { BackendKind } from "../bridge";
import type { AgentDefinitionStore } from "./store";
import type { AgentDefinitionEntry } from "./types";

export class AgentMenu {
  private container: HTMLElement;
  private store: AgentDefinitionStore;

  onSpawnAgent:
    | ((definitionId: string, backendOverride?: BackendKind) => void)
    | null = null;

  constructor(container: HTMLElement, store: AgentDefinitionStore) {
    this.container = container;
    this.container.classList.add("agent-menu-panel");
    this.store = store;
    this.render();
  }

  async refresh(): Promise<void> {
    await this.store.load();
    this.render();
  }

  render(): void {
    this.container.innerHTML = "";
    this.container.appendChild(this.buildToolbar());

    const definitions = this.store.getAll();
    if (definitions.length === 0) {
      this.container.appendChild(this.buildEmptyState());
      return;
    }

    const list = document.createElement("div");
    list.className = "agent-menu-list";

    for (const def of definitions) {
      list.appendChild(this.buildDefinitionCard(def));
    }

    this.container.appendChild(list);
  }

  private buildToolbar(): HTMLElement {
    const toolbar = document.createElement("div");
    toolbar.className = "agent-menu-toolbar";

    const title = document.createElement("span");
    title.className = "agent-menu-toolbar-title";
    title.textContent = "Agent Library";

    const refreshBtn = document.createElement("button");
    refreshBtn.type = "button";
    refreshBtn.className = "agent-menu-toolbar-btn";
    refreshBtn.textContent = "Refresh";
    refreshBtn.title = "Reload agent definitions from disk";
    refreshBtn.addEventListener("click", () => {
      void this.refresh();
    });

    toolbar.appendChild(title);
    toolbar.appendChild(refreshBtn);
    return toolbar;
  }

  private buildEmptyState(): HTMLElement {
    const empty = document.createElement("div");
    empty.className = "agent-menu-empty";
    empty.textContent = "No agent definitions found.";
    return empty;
  }

  private buildDefinitionCard(def: AgentDefinitionEntry): HTMLElement {
    const card = document.createElement("div");
    card.className = "agent-menu-card";
    card.dataset.testid = `agent-menu-card-${def.id}`;

    const header = document.createElement("div");
    header.className = "agent-menu-card-header";

    const name = document.createElement("span");
    name.className = "agent-menu-card-name";
    name.textContent = def.name;

    const scopeBadge = document.createElement("span");
    scopeBadge.className = `agent-menu-card-scope agent-menu-scope-${def.scope}`;
    scopeBadge.textContent = def.scope;

    header.appendChild(name);
    header.appendChild(scopeBadge);
    card.appendChild(header);

    if (def.description) {
      const desc = document.createElement("div");
      desc.className = "agent-menu-card-description";
      desc.textContent = def.description;
      card.appendChild(desc);
    }

    const actions = document.createElement("div");
    actions.className = "agent-menu-card-actions";

    const spawnBtn = document.createElement("button");
    spawnBtn.type = "button";
    spawnBtn.className = "agent-menu-card-spawn-btn";
    spawnBtn.textContent = "Spawn";
    spawnBtn.title = `Spawn a new ${def.name} agent`;
    spawnBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      this.onSpawnAgent?.(def.id);
    });

    actions.appendChild(spawnBtn);
    card.appendChild(actions);

    return card;
  }
}
