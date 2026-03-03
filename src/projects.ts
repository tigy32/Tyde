import type { Project, ProjectStateManager } from "./project_state";
import { promptForText } from "./text_prompt";

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

    // Project list
    const list = document.createElement("div");
    list.className = "rail-projects";

    for (const project of this.stateManager.projects) {
      const item = this.createProjectItem(project);
      list.appendChild(item);
    }

    this.container.appendChild(list);

    // Add button
    const addBtn = document.createElement("button");
    addBtn.className = "rail-add-btn";
    addBtn.dataset.testid = "rail-add-btn";
    addBtn.innerHTML = '+<span class="rail-project-name">Add Project</span>';
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

    // Click to switch
    item.addEventListener("click", () => this.onSwitchProject(project.id));

    // Context menu
    item.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      this.showContextMenu(e.clientX, e.clientY, project);
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

    const closeItem = document.createElement("div");
    closeItem.className = "rail-context-menu-item";
    closeItem.textContent = "Close Project";
    closeItem.addEventListener("click", () => {
      menu.remove();
      this.onRemoveProject(project.id);
    });

    menu.appendChild(renameItem);
    menu.appendChild(closeItem);
    document.body.appendChild(menu);

    // Dismiss on outside click
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
}
