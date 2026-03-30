import { type Host, listHosts } from "./bridge";
import type { Project, ProjectStateManager } from "./project_state";
import { promptForText } from "./text_prompt";
import { parseRemoteWorkspaceUri } from "./workspace";

const AVATAR_COLORS = [
  "#e91e63",
  "#9c27b0",
  "#673ab7",
  "#3f51b5",
  "#2196f3",
  "#00bcd4",
  "#009688",
  "#4caf50",
  "#ff9800",
  "#ff5722",
];

export class ProjectSidebar {
  private container: HTMLElement;
  private stateManager: ProjectStateManager;
  private onSwitchProject: (id: string) => void;
  private onSwitchToHome: () => void;
  private onAddProject: () => void;
  private onRemoveProject: (id: string) => void;
  onCreateWorkbench: ((parentProjectId: string) => void) | null = null;
  onRemoveWorkbench: ((projectId: string) => void) | null = null;
  onManageRoots: ((projectId: string) => void) | null = null;
  onAddRemoteProject: ((host: Host) => void) | null = null;
  private hosts: Host[] = [];

  constructor(
    container: HTMLElement,
    stateManager: ProjectStateManager,
    onSwitchProject: (id: string) => void,
    onSwitchToHome: () => void,
    onAddProject: () => void,
    onRemoveProject: (id: string) => void,
  ) {
    this.container = container;
    this.stateManager = stateManager;
    this.onSwitchProject = onSwitchProject;
    this.onSwitchToHome = onSwitchToHome;
    this.onAddProject = onAddProject;
    this.onRemoveProject = onRemoveProject;

    this.stateManager.onChange = () => this.render();
    this.render();
  }

  refreshHosts(): void {
    listHosts()
      .then((hosts) => {
        this.hosts = hosts;
        this.render();
      })
      .catch((err) => console.error("Failed to load hosts:", err));
  }

  render(): void {
    this.container.innerHTML = "";

    const collapsed = this.stateManager.sidebarCollapsed;

    // Collapse toggle
    const toggle = document.createElement("button");
    toggle.className = "rail-toggle";
    toggle.textContent = collapsed ? "›" : "‹";
    toggle.addEventListener("click", () => {
      const next = !this.stateManager.sidebarCollapsed;
      this.stateManager.sidebarCollapsed = next;
      this.stateManager.setSidebarCollapsed(next);
      this.container.classList.toggle("collapsed", next);
      this.render();
    });
    this.container.appendChild(toggle);

    // Home tab — always present at top
    const homeItem = document.createElement("div");
    homeItem.className = "rail-project-item rail-home-item";
    homeItem.dataset.testid = "rail-home-item";
    if (this.stateManager.isHomeActive()) {
      homeItem.classList.add("active");
    }

    const homeAvatar = document.createElement("div");
    homeAvatar.className = "rail-avatar rail-home-avatar";
    homeAvatar.textContent = "⌂";

    const homeLabel = document.createElement("span");
    homeLabel.className = "rail-project-name";
    homeLabel.dataset.testid = "rail-project-name";
    homeLabel.textContent = "All Projects";

    homeItem.appendChild(homeAvatar);
    homeItem.appendChild(homeLabel);
    homeItem.addEventListener("click", () => this.onSwitchToHome());
    this.container.appendChild(homeItem);

    const list = document.createElement("div");
    list.className = "rail-projects";

    const groupedByHost = this.groupProjectsByHost();

    const localGroup = groupedByHost.get("local") ?? [];
    list.appendChild(this.createHostGroupHeader("Local", null));
    for (const project of localGroup) {
      if (project.parentProjectId) continue;
      list.appendChild(this.createProjectItem(project));
      for (const workbench of this.stateManager.getWorkbenches(project.id)) {
        list.appendChild(this.createWorkbenchItem(workbench));
      }
    }

    const remoteHostnames = new Set<string>();
    for (const host of this.hosts) {
      if (!host.is_local && host.hostname) remoteHostnames.add(host.hostname);
    }
    for (const hostname of groupedByHost.keys()) {
      if (hostname !== "local") remoteHostnames.add(hostname);
    }

    const sortedRemoteHosts = Array.from(remoteHostnames).sort((a, b) =>
      a.localeCompare(b),
    );
    for (const hostname of sortedRemoteHosts) {
      const host = this.hosts.find((h) => h.hostname === hostname) ?? null;
      const label = host ? host.label : hostname;
      const projects = groupedByHost.get(hostname) ?? [];
      list.appendChild(this.createHostGroupHeader(label, host));
      if (projects.length === 0) {
        const empty = document.createElement("div");
        empty.className = "rail-host-empty";
        empty.textContent = "No open projects";
        list.appendChild(empty);
        continue;
      }
      for (const project of projects) {
        if (project.parentProjectId) continue;
        list.appendChild(this.createProjectItem(project));
        for (const workbench of this.stateManager.getWorkbenches(project.id)) {
          list.appendChild(this.createWorkbenchItem(workbench));
        }
      }
    }

    this.container.appendChild(list);

    const addBtn = document.createElement("button");
    addBtn.className = "rail-add-btn";
    addBtn.dataset.testid = "rail-add-btn";
    addBtn.innerHTML =
      '+<span class="rail-project-name">Add Local Project</span>';
    addBtn.addEventListener("click", () => this.onAddProject());
    this.container.appendChild(addBtn);

    if (collapsed) {
      this.container.classList.add("collapsed");
    } else {
      this.container.classList.remove("collapsed");
    }
  }

  private createProjectItem(project: Project): HTMLElement {
    const item = document.createElement("div");
    item.className = "rail-project-item";
    item.dataset.testid = "rail-project-item";
    item.dataset.projectId = project.id;

    if (project.id === this.stateManager.activeProjectId) {
      item.classList.add("active");
    }

    // Avatar
    const avatar = document.createElement("div");
    avatar.className = "rail-avatar";
    avatar.textContent = project.name.charAt(0).toUpperCase();
    const colorIndex = project.id.charCodeAt(0) % AVATAR_COLORS.length;
    avatar.style.background = AVATAR_COLORS[colorIndex];

    // Activity dot
    if (project.status !== "idle") {
      const dot = document.createElement("span");
      dot.className = `rail-activity-dot ${project.status}`;
      avatar.appendChild(dot);
    }

    item.appendChild(avatar);

    // Label
    const label = document.createElement("span");
    label.className = "rail-project-name";
    label.dataset.testid = "rail-project-name";
    label.textContent = project.name;
    item.appendChild(label);

    if (project.roots.length > 0) {
      const badge = document.createElement("span");
      badge.className = "rail-roots-badge";
      badge.textContent = String(project.roots.length);
      badge.title = project.roots.join(", ");
      item.appendChild(badge);
    }

    // Click to switch
    item.addEventListener("click", () => this.onSwitchProject(project.id));

    // Context menu
    item.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      this.showContextMenu(e.clientX, e.clientY, project);
    });

    return item;
  }

  private createWorkbenchItem(project: Project): HTMLElement {
    const item = document.createElement("div");
    item.className = "rail-project-item rail-workbench-item";
    item.dataset.testid = "rail-workbench-item";
    item.dataset.projectId = project.id;

    if (project.id === this.stateManager.activeProjectId) {
      item.classList.add("active");
    }

    // Workbench icon (branch-like indicator)
    const avatar = document.createElement("div");
    avatar.className = "rail-avatar rail-workbench-avatar";
    avatar.textContent = "⑂";
    const colorIndex = project.id.charCodeAt(0) % AVATAR_COLORS.length;
    avatar.style.background = AVATAR_COLORS[colorIndex];

    // Activity dot
    if (project.status !== "idle") {
      const dot = document.createElement("span");
      dot.className = `rail-activity-dot ${project.status}`;
      avatar.appendChild(dot);
    }

    item.appendChild(avatar);

    // Label
    const label = document.createElement("span");
    label.className = "rail-project-name";
    label.dataset.testid = "rail-project-name";
    label.textContent = project.name;
    item.appendChild(label);

    // Click to switch
    item.addEventListener("click", () => this.onSwitchProject(project.id));

    // Context menu
    item.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      this.showWorkbenchContextMenu(e.clientX, e.clientY, project);
    });

    return item;
  }

  private showContextMenu(x: number, y: number, project: Project): void {
    this.dismissContextMenu();

    const menu = document.createElement("div");
    menu.className = "rail-context-menu";
    menu.style.left = `${x}px`;
    menu.style.top = `${y}px`;

    const renameItem = document.createElement("div");
    renameItem.className = "rail-context-menu-item";
    renameItem.textContent = "Rename";
    renameItem.addEventListener("click", async () => {
      menu.remove();
      const name = await promptForText({
        title: "Project Name",
        defaultValue: project.name,
        placeholder: "Project name",
        confirmLabel: "Rename",
      });
      if (name === null) return;
      const trimmed = name.trim();
      if (!trimmed) return;
      this.stateManager.renameProject(project.id, trimmed);
    });

    const newWorkbenchItem = document.createElement("div");
    newWorkbenchItem.className = "rail-context-menu-item";
    newWorkbenchItem.dataset.testid = "rail-context-new-workbench";
    newWorkbenchItem.textContent = "New Workbench";
    newWorkbenchItem.addEventListener("click", () => {
      menu.remove();
      this.onCreateWorkbench?.(project.id);
    });

    const closeItem = document.createElement("div");
    closeItem.className = "rail-context-menu-item";
    closeItem.textContent = "Close Project";
    closeItem.addEventListener("click", () => {
      menu.remove();
      this.onRemoveProject(project.id);
    });

    const manageRootsItem = document.createElement("div");
    manageRootsItem.className = "rail-context-menu-item";
    manageRootsItem.dataset.testid = "rail-context-manage-roots";
    manageRootsItem.textContent = "Sub-Roots\u2026";
    manageRootsItem.addEventListener("click", () => {
      menu.remove();
      this.onManageRoots?.(project.id);
    });

    menu.appendChild(renameItem);
    menu.appendChild(manageRootsItem);
    menu.appendChild(newWorkbenchItem);
    menu.appendChild(closeItem);
    document.body.appendChild(menu);

    // Dismiss on outside click
    const dismiss = () => {
      menu.remove();
      document.removeEventListener("click", dismiss);
    };
    document.addEventListener("click", dismiss, { once: true });
  }

  private showWorkbenchContextMenu(
    x: number,
    y: number,
    project: Project,
  ): void {
    this.dismissContextMenu();

    const menu = document.createElement("div");
    menu.className = "rail-context-menu";
    menu.style.left = `${x}px`;
    menu.style.top = `${y}px`;

    const renameItem = document.createElement("div");
    renameItem.className = "rail-context-menu-item";
    renameItem.textContent = "Rename";
    renameItem.addEventListener("click", async () => {
      menu.remove();
      const name = await promptForText({
        title: "Workbench Name",
        defaultValue: project.name,
        placeholder: "Workbench name",
        confirmLabel: "Rename",
      });
      if (name === null) return;
      const trimmed = name.trim();
      if (!trimmed) return;
      this.stateManager.renameProject(project.id, trimmed);
    });

    const removeItem = document.createElement("div");
    removeItem.className =
      "rail-context-menu-item rail-context-menu-item-danger";
    removeItem.dataset.testid = "rail-context-remove-workbench";
    removeItem.textContent = "Remove Workbench";
    removeItem.addEventListener("click", () => {
      menu.remove();
      this.onRemoveWorkbench?.(project.id);
    });

    menu.appendChild(renameItem);
    menu.appendChild(removeItem);
    document.body.appendChild(menu);

    const dismiss = () => {
      menu.remove();
      document.removeEventListener("click", dismiss);
    };
    document.addEventListener("click", dismiss, { once: true });
  }

  private dismissContextMenu(): void {
    document
      .querySelectorAll(".rail-context-menu")
      .forEach((el) => el.remove());
  }

  private groupProjectsByHost(): Map<string, Project[]> {
    const groups = new Map<string, Project[]>();
    groups.set("local", []);
    for (const project of this.stateManager.projects) {
      const remote = parseRemoteWorkspaceUri(project.workspacePath);
      const key = remote ? remote.host : "local";
      const arr = groups.get(key) ?? [];
      arr.push(project);
      groups.set(key, arr);
    }
    return groups;
  }

  private createHostGroupHeader(label: string, host: Host | null): HTMLElement {
    const header = document.createElement("div");
    header.className = "rail-host-header";
    header.dataset.testid = "rail-host-header";

    const labelEl = document.createElement("span");
    labelEl.className = "rail-host-label";
    labelEl.textContent = label;
    header.appendChild(labelEl);

    if (host !== null && !host.is_local) {
      const addBtn = document.createElement("button");
      addBtn.className = "rail-host-add-btn";
      addBtn.dataset.testid = "rail-host-add-btn";
      addBtn.textContent = "+";
      addBtn.title = `Add project from ${label}`;
      addBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        this.onAddRemoteProject?.(host);
      });
      header.appendChild(addBtn);
    }

    return header;
  }
}
