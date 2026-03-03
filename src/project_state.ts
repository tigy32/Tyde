export type ProjectStatus = "idle" | "active" | "needs_attention";

export interface Project {
  id: string;
  name: string;
  workspacePath: string;
  conversationIds: number[];
  activeConversationId: number | null;
  status: ProjectStatus;
}

interface PersistedState {
  projects: Array<{
    id: string;
    name: string;
    workspacePath: string;
  }>;
  activeProjectId: string | null;
  sidebarCollapsed: boolean;
}

const STORAGE_KEY = "tyde-projects";

export class ProjectStateManager {
  projects: Project[] = [];
  activeProjectId: string | null = null;
  sidebarCollapsed = false;
  onChange: (() => void) | null = null;

  constructor() {
    this.restore();
  }

  addProject(workspacePath: string): Project {
    const name =
      workspacePath.split("/").pop() ||
      workspacePath.split("\\").pop() ||
      workspacePath;
    const project: Project = {
      id: crypto.randomUUID(),
      name,
      workspacePath,
      conversationIds: [],
      activeConversationId: null,
      status: "idle",
    };
    this.projects.push(project);
    this.onChange?.();
    this.persist();
    return project;
  }

  removeProject(id: string): void {
    this.projects = this.projects.filter((p) => p.id !== id);
    if (this.activeProjectId === id) {
      this.activeProjectId = this.projects[0]?.id ?? null;
    }
    this.onChange?.();
    this.persist();
  }

  switchProject(id: string): void {
    this.activeProjectId = id;
    const project = this.projects.find((p) => p.id === id);
    if (project && project.status === "needs_attention") {
      project.status = "idle";
    }
    this.onChange?.();
    this.persist();
  }

  switchToHome(): void {
    this.activeProjectId = null;
    this.onChange?.();
    this.persist();
  }

  isHomeActive(): boolean {
    return this.activeProjectId === null;
  }

  getActiveProject(): Project | null {
    if (!this.activeProjectId) return null;
    return this.projects.find((p) => p.id === this.activeProjectId) ?? null;
  }

  updateProjectStatus(id: string, status: ProjectStatus): void {
    const project = this.projects.find((p) => p.id === id);
    if (!project) return;
    project.status = status;
    this.onChange?.();
    this.persist();
  }

  renameProject(id: string, name: string): void {
    const project = this.projects.find((p) => p.id === id);
    if (!project) return;
    project.name = name;
    this.onChange?.();
    this.persist();
  }

  addConversationToProject(projectId: string, conversationId: number): void {
    const project = this.projects.find((p) => p.id === projectId);
    if (!project) return;
    project.conversationIds.push(conversationId);
    this.onChange?.();
  }

  setProjectConversationIds(
    projectId: string,
    conversationIds: number[],
  ): void {
    const project = this.projects.find((p) => p.id === projectId);
    if (!project) return;
    const next = Array.from(new Set(conversationIds));
    if (
      project.conversationIds.length === next.length &&
      project.conversationIds.every((id, idx) => id === next[idx])
    ) {
      return;
    }
    project.conversationIds = next;
    this.onChange?.();
  }

  removeConversationFromProject(
    projectId: string,
    conversationId: number,
  ): void {
    const project = this.projects.find((p) => p.id === projectId);
    if (!project) return;
    project.conversationIds = project.conversationIds.filter(
      (id) => id !== conversationId,
    );
    this.onChange?.();
  }

  findProjectByConversationId(conversationId: number): Project | null {
    return (
      this.projects.find((p) => p.conversationIds.includes(conversationId)) ??
      null
    );
  }

  setSidebarCollapsed(collapsed: boolean): void {
    this.sidebarCollapsed = collapsed;
    this.persist();
  }

  persist(): void {
    const state: PersistedState = {
      projects: this.projects.map((p) => ({
        id: p.id,
        name: p.name,
        workspacePath: p.workspacePath,
      })),
      activeProjectId: this.activeProjectId,
      sidebarCollapsed: this.sidebarCollapsed,
    };
    localStorage.setItem(STORAGE_KEY, JSON.stringify(state));
  }

  restore(): void {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) {
      this.projects = [];
      this.activeProjectId = null;
      return;
    }
    let state: PersistedState;
    try {
      state = JSON.parse(raw);
    } catch (err) {
      console.error("Failed to parse persisted project state, resetting:", err);
      localStorage.removeItem(STORAGE_KEY);
      return;
    }
    this.projects = state.projects.map((p) => ({
      id: p.id,
      name: p.name,
      workspacePath: p.workspacePath,
      conversationIds: [],
      activeConversationId: null,
      status: "idle" as const,
    }));
    this.activeProjectId = state.activeProjectId;
    this.sidebarCollapsed = state.sidebarCollapsed;
  }
}
