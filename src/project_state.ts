export type ProjectStatus = "idle" | "active" | "needs_attention";

export type WorkbenchKind = "git-worktree";

export interface Project {
  id: string;
  name: string;
  workspacePath: string;
  roots: string[];
  conversationIds: number[];
  activeConversationId: number | null;
  status: ProjectStatus;
  parentProjectId: string | null;
  workbenchKind: WorkbenchKind | null;
}

interface PersistedState {
  projects: Array<{
    id: string;
    name: string;
    workspacePath: string;
    roots?: string[];
    parentProjectId?: string | null;
    workbenchKind?: WorkbenchKind | null;
  }>;
  activeProjectId: string | null;
  sidebarCollapsed: boolean;
}

const STORAGE_KEY = "tyde-projects";

function normalizeRoots(raw: unknown): string[] {
  if (!Array.isArray(raw)) return [];
  const seen = new Set<string>();
  const roots: string[] = [];
  for (const entry of raw) {
    if (typeof entry !== "string") continue;
    const trimmed = entry.trim();
    if (!trimmed || seen.has(trimmed)) continue;
    seen.add(trimmed);
    roots.push(trimmed);
  }
  return roots;
}

function normalizeWorkbenchKind(raw: unknown): WorkbenchKind | null {
  return raw === "git-worktree" ? raw : null;
}

export class ProjectStateManager {
  projects: Project[] = [];
  activeProjectId: string | null = null;
  sidebarCollapsed = false;
  onChange: (() => void) | null = null;

  constructor() {
    this.restore();
  }

  createProject(workspacePath: string): Project {
    const name =
      workspacePath.split("/").pop() ||
      workspacePath.split("\\").pop() ||
      workspacePath;
    const project: Project = {
      id: crypto.randomUUID(),
      name,
      workspacePath,
      roots: [],
      conversationIds: [],
      activeConversationId: null,
      status: "idle",
      parentProjectId: null,
      workbenchKind: null,
    };
    this.projects.push(project);
    this.onChange?.();
    return project;
  }

  commitProject(_project: Project): void {
    this.persist();
  }

  abandonProject(projectId: string): void {
    this.projects = this.projects.filter((p) => p.id !== projectId);
    this.onChange?.();
  }

  addProject(workspacePath: string): Project {
    const project = this.createProject(workspacePath);
    this.commitProject(project);
    return project;
  }

  addWorkbench(
    parentProjectId: string,
    workspacePath: string,
    name: string,
    kind: WorkbenchKind,
  ): Project {
    const parent = this.projects.find((p) => p.id === parentProjectId);
    const project: Project = {
      id: crypto.randomUUID(),
      name,
      workspacePath,
      roots: parent?.roots?.slice() ?? [],
      conversationIds: [],
      activeConversationId: null,
      status: "idle",
      parentProjectId,
      workbenchKind: kind,
    };
    // Insert right after the parent and its existing workbenches
    const parentIndex = this.projects.findIndex(
      (p) => p.id === parentProjectId,
    );
    let insertIndex = parentIndex + 1;
    while (
      insertIndex < this.projects.length &&
      this.projects[insertIndex].parentProjectId === parentProjectId
    ) {
      insertIndex++;
    }
    this.projects.splice(insertIndex, 0, project);
    this.onChange?.();
    this.persist();
    return project;
  }

  getWorkbenches(parentId: string): Project[] {
    return this.projects.filter((p) => p.parentProjectId === parentId);
  }

  isWorkbench(id: string): boolean {
    const project = this.projects.find((p) => p.id === id);
    return (
      project?.parentProjectId !== null &&
      project?.parentProjectId !== undefined
    );
  }

  getParentProject(id: string): Project | null {
    const project = this.projects.find((p) => p.id === id);
    if (!project?.parentProjectId) return null;
    return this.projects.find((p) => p.id === project.parentProjectId) ?? null;
  }

  removeProject(id: string): void {
    // Also remove child workbenches when removing a parent
    const childIds = this.projects
      .filter((p) => p.parentProjectId === id)
      .map((p) => p.id);
    const removeIds = new Set([id, ...childIds]);
    this.projects = this.projects.filter((p) => !removeIds.has(p.id));
    if (removeIds.has(this.activeProjectId!)) {
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

  addProjectRoot(projectId: string, root: string): void {
    const project = this.projects.find((p) => p.id === projectId);
    if (!project) return;
    if (project.roots.includes(root)) return;
    project.roots.push(root);
    this.onChange?.();
    this.persist();
  }

  removeProjectRoot(projectId: string, root: string): void {
    const project = this.projects.find((p) => p.id === projectId);
    if (!project) return;
    project.roots = project.roots.filter((r) => r !== root);
    this.onChange?.();
    this.persist();
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
        roots: p.roots,
        parentProjectId: p.parentProjectId,
        workbenchKind: p.workbenchKind,
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
    let state: unknown;
    try {
      state = JSON.parse(raw);
    } catch (err) {
      console.error("Failed to parse persisted project state, resetting:", err);
      localStorage.removeItem(STORAGE_KEY);
      this.projects = [];
      this.activeProjectId = null;
      this.sidebarCollapsed = false;
      return;
    }
    if (!state || typeof state !== "object") {
      this.projects = [];
      this.activeProjectId = null;
      this.sidebarCollapsed = false;
      return;
    }

    const parsed = state as Partial<PersistedState>;
    const projectsRaw = Array.isArray(parsed.projects) ? parsed.projects : [];
    const projects: Project[] = [];
    const seenIds = new Set<string>();

    for (const entry of projectsRaw) {
      if (!entry || typeof entry !== "object") continue;
      const project = entry as Partial<PersistedState["projects"][number]>;
      const workspacePath =
        typeof project.workspacePath === "string"
          ? project.workspacePath.trim()
          : "";
      if (!workspacePath) continue;

      const candidateId =
        typeof project.id === "string" && project.id.trim().length > 0
          ? project.id
          : crypto.randomUUID();
      const id = seenIds.has(candidateId) ? crypto.randomUUID() : candidateId;
      seenIds.add(id);

      const name =
        typeof project.name === "string" && project.name.trim().length > 0
          ? project.name
          : workspacePath.split("/").pop() ||
            workspacePath.split("\\").pop() ||
            workspacePath;

      const parentProjectId =
        typeof project.parentProjectId === "string" &&
        project.parentProjectId.trim().length > 0
          ? project.parentProjectId
          : null;

      projects.push({
        id,
        name,
        workspacePath,
        roots: normalizeRoots(project.roots),
        conversationIds: [],
        activeConversationId: null,
        status: "idle",
        parentProjectId,
        workbenchKind: normalizeWorkbenchKind(project.workbenchKind),
      });
    }

    this.projects = projects;

    const activeProjectId =
      typeof parsed.activeProjectId === "string"
        ? parsed.activeProjectId
        : null;
    this.activeProjectId =
      activeProjectId && this.projects.some((p) => p.id === activeProjectId)
        ? activeProjectId
        : null;
    this.sidebarCollapsed =
      typeof parsed.sidebarCollapsed === "boolean"
        ? parsed.sidebarCollapsed
        : false;
  }
}
