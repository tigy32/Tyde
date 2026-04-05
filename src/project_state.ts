import {
  addProject as addProjectCmd,
  addProjectWorkbench as addProjectWorkbenchCmd,
  listProjects,
  type ProjectRecord,
  type ProjectsChangedPayload,
  removeProject as removeProjectCmd,
  renameProjectRecord,
  updateProjectRoots,
} from "./bridge";

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

const UI_STATE_KEY = "tyde-projects-ui";
const LEGACY_STORAGE_KEY = "tyde-projects";

function normalizeWorkbenchKind(raw: unknown): WorkbenchKind | null {
  return raw === "git-worktree" ? raw : null;
}

function projectFromRecord(record: ProjectRecord): Project {
  return {
    id: record.id,
    name: record.name,
    workspacePath: record.workspace_path,
    roots: record.roots ?? [],
    conversationIds: [],
    activeConversationId: null,
    status: "idle",
    parentProjectId: record.parent_project_id ?? null,
    workbenchKind: normalizeWorkbenchKind(record.workbench_kind),
  };
}

export class ProjectStateManager {
  projects: Project[] = [];
  activeProjectId: string | null = null;
  sidebarCollapsed = false;
  onChange: (() => void) | null = null;

  constructor() {
    this.restoreUiState();
  }

  /** Load projects from the server-side store. Called once on startup. */
  async loadFromServer(): Promise<void> {
    const records = await listProjects();
    this.applyServerRecords({ projects: records });
  }

  /** Load projects from a remote server-side store. */
  async loadFromRemoteServer(host: string): Promise<void> {
    const records = await listProjects(host);
    this.applyServerRecords({ projects: records, host });
  }

  /** Migrate projects from legacy localStorage to server-side store, then clear. */
  async migrateFromLocalStorage(): Promise<void> {
    const raw = localStorage.getItem(LEGACY_STORAGE_KEY);
    if (!raw) return;

    let parsed: {
      projects?: Array<{
        workspacePath?: string;
        name?: string;
        roots?: string[];
        parentProjectId?: string | null;
        workbenchKind?: string | null;
      }>;
    } | null = null;
    try {
      parsed = JSON.parse(raw);
    } catch {
      localStorage.removeItem(LEGACY_STORAGE_KEY);
      return;
    }
    if (!parsed || !Array.isArray(parsed.projects)) {
      localStorage.removeItem(LEGACY_STORAGE_KEY);
      return;
    }

    // Import each project, parents first then workbenches
    const parents = parsed.projects.filter((p) => !p.parentProjectId);
    const workbenches = parsed.projects.filter((p) => p.parentProjectId);

    // Map old ids to new server-side ids
    const idMap = new Map<string, string>();
    let hadFailure = false;

    for (const p of parents) {
      if (!p.workspacePath) continue;
      const name =
        p.name || p.workspacePath.split("/").pop() || p.workspacePath;
      try {
        const record = await addProjectCmd(p.workspacePath, name);
        // Store mapping from old to new ID for workbench migration
        const oldEntry = parsed.projects.find(
          (e) => e.workspacePath === p.workspacePath,
        );
        if (oldEntry && (oldEntry as { id?: string }).id) {
          idMap.set((oldEntry as { id: string }).id, record.id);
        }
        // Migrate roots if any
        if (Array.isArray(p.roots) && p.roots.length > 0) {
          await updateProjectRoots(record.id, p.roots);
        }
      } catch (err) {
        console.error("Migration: failed to add project", p.workspacePath, err);
        hadFailure = true;
      }
    }

    for (const p of workbenches) {
      if (!p.workspacePath || !p.parentProjectId) continue;
      const parentId = idMap.get(p.parentProjectId) ?? p.parentProjectId;
      const name =
        p.name || p.workspacePath.split("/").pop() || p.workspacePath;
      const kind = p.workbenchKind || "git-worktree";
      try {
        await addProjectWorkbenchCmd(parentId, p.workspacePath, name, kind);
      } catch (err) {
        console.error(
          "Migration: failed to add workbench",
          p.workspacePath,
          err,
        );
        hadFailure = true;
      }
    }

    if (hadFailure) {
      console.warn(
        "Migration: some projects failed to import; keeping legacy localStorage for retry",
      );
      return;
    }
    localStorage.removeItem(LEGACY_STORAGE_KEY);

    // Migrate activeProjectId and sidebarCollapsed if they were in the legacy state
    const oldActiveId = (parsed as any).activeProjectId;
    if (oldActiveId && idMap.has(oldActiveId)) {
      this.activeProjectId = idMap.get(oldActiveId)!;
    }
    const oldSidebarCollapsed = (parsed as any).sidebarCollapsed;
    if (typeof oldSidebarCollapsed === "boolean") {
      this.sidebarCollapsed = oldSidebarCollapsed;
    }
    this.persistUiState();
  }

  /**
   * Apply a fresh list of ProjectRecords from a server (local or remote).
   * Preserves client-only state (conversationIds, status, activeConversationId).
   * Merges with existing projects from other hosts.
   */
  applyServerRecords(payload: ProjectsChangedPayload): void {
    if (!payload || !Array.isArray(payload.projects)) {
      console.warn("applyServerRecords: invalid payload", payload);
      return;
    }
    const { projects: records, host } = payload;
    const recordsById = new Map(
      records.filter((r) => r !== null).map((r) => [r.id, r]),
    );

    // Identify which existing projects belong to this source.
    // Local projects have no ssh:// prefix. Remote projects have ssh://host/ prefix.
    const isFromThisSource = (project: Project): boolean => {
      if (!host) {
        return !project.workspacePath.startsWith("ssh://");
      }
      // For remote, the records already have ssh://host/ prefix from normalization
      return project.workspacePath.startsWith(`ssh://${host}/`);
    };

    const updatedProjects: Project[] = [];

    // Keep projects from OTHER sources
    for (const p of this.projects) {
      if (!isFromThisSource(p)) {
        updatedProjects.push(p);
      } else {
        // If it's from this source, we'll update it from the new records
        const record = recordsById.get(p.id);
        if (record) {
          const updated = projectFromRecord(record);
          updated.conversationIds = p.conversationIds;
          updated.activeConversationId = p.activeConversationId;
          updated.status = p.status;
          updatedProjects.push(updated);
          recordsById.delete(p.id); // Done with this record
        }
        // If not in recordsById, it was removed on the server; don't push it
      }
    }

    // Add remaining NEW records from this source
    for (const record of recordsById.values()) {
      updatedProjects.push(projectFromRecord(record));
    }

    this.projects = updatedProjects;

    // Validate activeProjectId
    if (
      this.activeProjectId &&
      !this.projects.some((p) => p.id === this.activeProjectId)
    ) {
      this.activeProjectId = null;
    }
    this.persistUiState();
    this.onChange?.();
  }

  /**
   * Create an unpersisted project (used by remote workspace flow).
   * Call commitProject() after successful connection.
   */
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

  /** Persist a previously-created project to the server store. */
  async commitProject(project: Project, host?: string): Promise<void> {
    const record = await addProjectCmd(
      project.workspacePath,
      project.name,
      host,
    );
    // Update the in-memory project with the server-assigned ID
    const idx = this.projects.findIndex((p) => p.id === project.id);
    if (idx !== -1) {
      const old = this.projects[idx];
      old.id = record.id;
      // Update activeProjectId if it pointed to the old ID
      if (this.activeProjectId === project.id) {
        this.activeProjectId = record.id;
      }
    }
    this.persistUiState();
  }

  abandonProject(projectId: string): void {
    this.projects = this.projects.filter((p) => p.id !== projectId);
    this.onChange?.();
  }

  async addProject(workspacePath: string, host?: string): Promise<Project> {
    const name =
      workspacePath.split("/").pop() ||
      workspacePath.split("\\").pop() ||
      workspacePath;
    const record = await addProjectCmd(workspacePath, name, host);
    if (!record) {
      throw new Error(`Failed to add project: backend returned null`);
    }
    const project = projectFromRecord(record);
    // Avoid duplicate if server event already added it
    if (!this.projects.some((p) => p.id === project.id)) {
      this.projects.push(project);
      this.onChange?.();
    }
    return this.projects.find((p) => p.id === project.id) ?? project;
  }

  async addWorkbench(
    parentProjectId: string,
    workspacePath: string,
    name: string,
    kind: WorkbenchKind,
    host?: string,
  ): Promise<Project> {
    const record = await addProjectWorkbenchCmd(
      parentProjectId,
      workspacePath,
      name,
      kind,
      host,
    );
    const project = projectFromRecord(record);
    // Insert after parent and its existing workbenches, if not already present
    if (!this.projects.some((p) => p.id === project.id)) {
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
    }
    this.persistUiState();
    return this.projects.find((p) => p.id === project.id) ?? project;
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

  private getHostForProject(project: Project): string | undefined {
    if (project.workspacePath.startsWith("ssh://")) {
      const parts = project.workspacePath.slice(6).split("/");
      return parts[0];
    }
    return undefined;
  }

  async removeProject(id: string): Promise<void> {
    const project = this.projects.find((p) => p.id === id);
    if (project) {
      await removeProjectCmd(id, this.getHostForProject(project));
    }
    // Also remove child workbenches from local state
    const childIds = this.projects
      .filter((p) => p.parentProjectId === id)
      .map((p) => p.id);
    const removeIds = new Set([id, ...childIds]);
    this.projects = this.projects.filter((p) => !removeIds.has(p.id));
    if (removeIds.has(this.activeProjectId!)) {
      this.activeProjectId = this.projects[0]?.id ?? null;
    }
    this.onChange?.();
    this.persistUiState();
  }

  switchProject(id: string): void {
    this.activeProjectId = id;
    const project = this.projects.find((p) => p.id === id);
    if (project && project.status === "needs_attention") {
      project.status = "idle";
    }
    this.onChange?.();
    this.persistUiState();
  }

  switchToHome(): void {
    this.activeProjectId = null;
    this.onChange?.();
    this.persistUiState();
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
  }

  async renameProject(id: string, name: string): Promise<void> {
    const project = this.projects.find((p) => p.id === id);
    if (project) {
      await renameProjectRecord(id, name, this.getHostForProject(project));
      project.name = name;
    }
    this.onChange?.();
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

  async addProjectRoot(projectId: string, root: string): Promise<void> {
    const project = this.projects.find((p) => p.id === projectId);
    if (!project) return;
    if (project.roots.includes(root)) return;
    const newRoots = [...project.roots, root];
    await updateProjectRoots(
      projectId,
      newRoots,
      this.getHostForProject(project),
    );
    project.roots = newRoots;
    this.onChange?.();
  }

  async removeProjectRoot(projectId: string, root: string): Promise<void> {
    const project = this.projects.find((p) => p.id === projectId);
    if (!project) return;
    const newRoots = project.roots.filter((r) => r !== root);
    await updateProjectRoots(
      projectId,
      newRoots,
      this.getHostForProject(project),
    );
    project.roots = newRoots;
    this.onChange?.();
  }

  setSidebarCollapsed(collapsed: boolean): void {
    this.sidebarCollapsed = collapsed;
    this.persistUiState();
  }

  private persistUiState(): void {
    const state = {
      activeProjectId: this.activeProjectId,
      sidebarCollapsed: this.sidebarCollapsed,
    };
    localStorage.setItem(UI_STATE_KEY, JSON.stringify(state));
  }

  private restoreUiState(): void {
    const raw = localStorage.getItem(UI_STATE_KEY);
    if (!raw) return;
    try {
      const state = JSON.parse(raw) as {
        activeProjectId?: string | null;
        sidebarCollapsed?: boolean;
      };
      this.activeProjectId =
        typeof state.activeProjectId === "string"
          ? state.activeProjectId
          : null;
      this.sidebarCollapsed =
        typeof state.sidebarCollapsed === "boolean"
          ? state.sidebarCollapsed
          : false;
    } catch {
      // Corrupted, ignore
    }
  }
}
