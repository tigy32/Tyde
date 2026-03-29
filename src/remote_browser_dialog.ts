import { type Host, listDirectory } from "./bridge";

export class RemoteBrowserDialog {
  private overlay: HTMLElement | null = null;
  private currentPath = "";
  private host: Host;
  private onSelect: (sshUri: string) => void;
  private listEl: HTMLElement | null = null;
  private pathDisplay: HTMLElement | null = null;
  private selectBtn: HTMLButtonElement | null = null;
  private loading = false;

  constructor(host: Host, onSelect: (sshUri: string) => void) {
    this.host = host;
    this.onSelect = onSelect;
  }

  show(): void {
    this.currentPath = "/";
    this.buildDOM();
    this.loadDirectory();
  }

  private buildDOM(): void {
    if (this.overlay) this.overlay.remove();

    this.overlay = document.createElement("div");
    this.overlay.className = "text-prompt-overlay";
    this.overlay.dataset.testid = "remote-browser-dialog";

    const card = document.createElement("div");
    card.className = "text-prompt-card remote-browser-card";
    card.setAttribute("role", "dialog");
    card.setAttribute("aria-modal", "true");
    card.setAttribute("aria-label", `Browse ${this.host.label}`);

    const title = document.createElement("h3");
    title.className = "text-prompt-title";
    title.textContent = `Browse: ${this.host.label}`;
    card.appendChild(title);

    const subtitle = document.createElement("p");
    subtitle.className = "text-prompt-description";
    subtitle.textContent = this.host.hostname;
    card.appendChild(subtitle);

    this.pathDisplay = document.createElement("div");
    this.pathDisplay.className = "remote-browser-path";
    this.pathDisplay.dataset.testid = "remote-browser-path";
    card.appendChild(this.pathDisplay);

    this.listEl = document.createElement("div");
    this.listEl.className = "remote-browser-list";
    this.listEl.dataset.testid = "remote-browser-list";
    card.appendChild(this.listEl);

    const actions = document.createElement("div");
    actions.className = "text-prompt-actions";

    const cancelBtn = document.createElement("button");
    cancelBtn.type = "button";
    cancelBtn.className = "text-prompt-btn";
    cancelBtn.textContent = "Cancel";
    cancelBtn.addEventListener("click", () => this.dismiss());

    this.selectBtn = document.createElement("button");
    this.selectBtn.type = "button";
    this.selectBtn.className = "text-prompt-btn text-prompt-btn-primary";
    this.selectBtn.dataset.testid = "remote-browser-select";
    this.selectBtn.textContent = "Open Project Here";
    this.selectBtn.addEventListener("click", () => this.selectCurrent());

    actions.appendChild(cancelBtn);
    actions.appendChild(this.selectBtn);
    card.appendChild(actions);

    this.overlay.appendChild(card);

    this.overlay.addEventListener("click", (e) => {
      if (e.target === this.overlay) this.dismiss();
    });
    card.addEventListener("keydown", (e) => {
      if (e.key === "Escape") {
        e.preventDefault();
        this.dismiss();
      }
    });

    document.body.appendChild(this.overlay);
  }

  private loadDirectory(): void {
    if (this.loading) return;
    this.loading = true;

    if (this.pathDisplay) {
      this.pathDisplay.textContent = this.currentPath;
    }
    if (this.listEl) {
      this.listEl.innerHTML =
        '<div class="panel-loading"><span class="loading-spinner"></span>Loading...</div>';
    }

    const remotePath = `ssh://${this.host.hostname}${this.currentPath}`;
    listDirectory(remotePath, false)
      .then((entries) => {
        this.renderEntries(entries);
      })
      .catch((err) => {
        if (!this.listEl) return;
        this.listEl.innerHTML = "";
        const errEl = document.createElement("div");
        errEl.className = "remote-browser-error";
        errEl.textContent = `Failed to list directory: ${String(err)}`;
        this.listEl.appendChild(errEl);
      })
      .finally(() => {
        this.loading = false;
      });
  }

  private renderEntries(
    entries: {
      name: string;
      path: string;
      is_directory: boolean;
      size: number | null;
    }[],
  ): void {
    if (!this.listEl) return;
    this.listEl.innerHTML = "";

    if (this.currentPath !== "/") {
      const upRow = document.createElement("div");
      upRow.className = "remote-browser-row";
      upRow.dataset.testid = "remote-browser-row";
      upRow.textContent = "📁 ..";
      upRow.addEventListener("click", () => this.navigateUp());
      this.listEl.appendChild(upRow);
    }

    const dirs = entries.filter((e) => e.is_directory);

    if (dirs.length === 0) {
      const empty = document.createElement("div");
      empty.className = "remote-browser-empty";
      empty.textContent = "No subdirectories";
      this.listEl.appendChild(empty);
      return;
    }

    for (const dir of dirs) {
      const row = document.createElement("div");
      row.className = "remote-browser-row";
      row.dataset.testid = "remote-browser-row";
      row.textContent = `📁 ${dir.name}`;
      row.addEventListener("click", () => this.navigateTo(dir.name));
      this.listEl.appendChild(row);
    }
  }

  private navigateTo(dirName: string): void {
    if (this.currentPath === "/") {
      this.currentPath = `/${dirName}`;
    } else {
      this.currentPath = `${this.currentPath}/${dirName}`;
    }
    this.loadDirectory();
  }

  private navigateUp(): void {
    const parts = this.currentPath.split("/").filter(Boolean);
    parts.pop();
    this.currentPath = parts.length === 0 ? "/" : `/${parts.join("/")}`;
    this.loadDirectory();
  }

  private selectCurrent(): void {
    const sshUri = `ssh://${this.host.hostname}${this.currentPath}`;
    this.onSelect(sshUri);
    this.dismiss();
  }

  dismiss(): void {
    if (!this.overlay) return;
    this.overlay.remove();
    this.overlay = null;
  }
}
