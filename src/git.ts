import type { GitFileStatus } from "@tyde/protocol";
import { applyPatch } from "diff";
import {
  gitCommit,
  gitCurrentBranch,
  gitDiff,
  gitDiffBaseContent,
  gitDiscard,
  gitStage,
  gitStatus,
  gitUnstage,
} from "./bridge";
import { escapeHtml } from "./renderer";

export class GitPanel {
  private container: HTMLElement;
  private workingDir = "";
  private currentBranch = "";
  private stagedFiles: GitFileStatus[] = [];
  private changedFiles: GitFileStatus[] = [];
  private untrackedFiles: GitFileStatus[] = [];
  private collapsedSections = new Set<string>();
  private refreshTimer: ReturnType<typeof setTimeout> | null = null;
  private lastRefreshTime = 0;
  private pollInterval: ReturnType<typeof setInterval> | null = null;
  private commitPrefix = "";

  onShowDiff:
    | ((diff: string, path: string, before?: string, after?: string) => void)
    | null = null;
  onError: ((message: string) => void) | null = null;

  constructor(container: HTMLElement) {
    this.container = container;
  }

  setWorkingDir(dir: string): void {
    this.workingDir = dir;
    this.refresh();
  }

  requestRefresh(): void {
    const now = Date.now();
    const elapsed = now - this.lastRefreshTime;
    if (this.refreshTimer) clearTimeout(this.refreshTimer);
    if (elapsed < 2000) {
      this.refreshTimer = setTimeout(() => this.refresh(), 2000 - elapsed);
      return;
    }
    this.refresh();
  }

  startPeriodicRefresh(): void {
    this.stopPeriodicRefresh();
    this.pollInterval = setInterval(() => this.requestRefresh(), 30000);
  }

  stopPeriodicRefresh(): void {
    if (this.pollInterval) {
      clearInterval(this.pollInterval);
      this.pollInterval = null;
    }
  }

  async refresh(): Promise<void> {
    this.lastRefreshTime = Date.now();

    if (!this.workingDir) {
      this.container.innerHTML =
        '<div class="git-empty">No workspace selected</div>';
      return;
    }

    this.container.innerHTML =
      '<div class="panel-loading"><span class="loading-spinner"></span>Loading git status...</div>';

    try {
      const [files, branch] = await Promise.all([
        gitStatus(this.workingDir),
        gitCurrentBranch(this.workingDir),
      ]);
      this.currentBranch = branch;
      this.stagedFiles = files.filter((f) => f.staged);
      this.changedFiles = files.filter(
        (f) => !f.staged && f.status !== "Untracked",
      );
      this.untrackedFiles = files.filter(
        (f) => !f.staged && f.status === "Untracked",
      );
      this.render();
    } catch (err) {
      const errMsg = String(err);
      if (errMsg.includes("not a git repository")) {
        this.container.innerHTML =
          '<div class="git-empty" data-testid="git-empty">Not a git repository</div>';
        this.stopPeriodicRefresh();
        return;
      }
      this.container.innerHTML = `<div class="git-error" data-testid="git-error">Git error: ${escapeHtml(errMsg)}</div>`;
      this.onError?.(`Git status failed: ${errMsg}`);
    }
  }

  private render(): void {
    this.container.innerHTML = "";

    this.container.appendChild(this.renderBranchDisplay());

    const totalFiles =
      this.stagedFiles.length +
      this.changedFiles.length +
      this.untrackedFiles.length;
    if (totalFiles === 0) {
      const empty = document.createElement("div");
      empty.className = "panel-empty-state";
      empty.dataset.testid = "git-clean";
      empty.innerHTML =
        '<span class="panel-empty-state-icon">✓</span><span>Working tree clean</span>';
      this.container.appendChild(empty);
      return;
    }

    this.container.appendChild(
      this.renderSection("Staged Changes", "staged", this.stagedFiles, true, {
        label: "Unstage All",
        handler: () => this.unstageAll(this.stagedFiles.map((f) => f.path)),
      }),
    );

    this.container.appendChild(
      this.renderSection("Changes", "changed", this.changedFiles, false, {
        label: "Stage All",
        handler: () => this.stageAll(this.changedFiles.map((f) => f.path)),
      }),
    );

    this.container.appendChild(
      this.renderSection("Untracked", "untracked", this.untrackedFiles, false, {
        label: "Stage All",
        handler: () => this.stageAll(this.untrackedFiles.map((f) => f.path)),
      }),
    );

    this.container.appendChild(this.renderCommitArea());
  }

  private renderBranchDisplay(): HTMLElement {
    const div = document.createElement("div");
    div.className = "git-branch-display";
    div.innerHTML = `<span class="git-branch-icon">⎇</span><span class="git-branch-name">${escapeHtml(this.currentBranch)}</span>`;
    return div;
  }

  private renderSection(
    title: string,
    sectionKey: string,
    files: GitFileStatus[],
    isStaged: boolean,
    bulkAction: { label: string; handler: () => void },
  ): HTMLElement {
    const section = document.createElement("div");
    section.className = "git-section";
    section.setAttribute("role", "region");
    section.setAttribute("aria-label", title);

    const collapsed = this.collapsedSections.has(sectionKey);

    const header = document.createElement("div");
    header.className = "git-section-header";
    header.setAttribute("aria-expanded", String(!collapsed));

    const chevron = document.createElement("span");
    chevron.className = "git-section-chevron";
    chevron.textContent = collapsed ? "▸" : "▾";

    const titleSpan = document.createElement("span");
    titleSpan.className = "git-section-title";
    titleSpan.textContent = `${title} (${files.length})`;

    header.appendChild(chevron);
    header.appendChild(titleSpan);

    if (files.length > 0) {
      const actionBtn = document.createElement("button");
      actionBtn.className = "git-section-action";
      actionBtn.textContent = bulkAction.label;
      actionBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        bulkAction.handler();
      });
      header.appendChild(actionBtn);
    }

    header.addEventListener("click", () => {
      if (this.collapsedSections.has(sectionKey)) {
        this.collapsedSections.delete(sectionKey);
      } else {
        this.collapsedSections.add(sectionKey);
      }
      header.setAttribute(
        "aria-expanded",
        String(!this.collapsedSections.has(sectionKey)),
      );
      this.render();
    });

    section.appendChild(header);

    if (!collapsed && files.length > 0) {
      const list = document.createElement("div");
      list.className = "git-file-list";
      for (const file of files) {
        list.appendChild(this.renderFileRow(file, isStaged));
      }
      section.appendChild(list);
    }

    return section;
  }

  private renderFileRow(file: GitFileStatus, isStaged: boolean): HTMLElement {
    const row = document.createElement("div");
    row.className = "git-file-row";
    row.setAttribute("role", "listitem");

    const statusIcon = document.createElement("span");
    statusIcon.className = `git-status-icon git-status-${file.status.toLowerCase()}`;
    statusIcon.textContent = statusLetter(file.status);

    const pathEl = document.createElement("span");
    pathEl.className = "git-file-path";
    pathEl.title = file.path;
    const lastSlash = file.path.lastIndexOf("/");
    if (lastSlash > -1) {
      const dirEl = document.createElement("span");
      dirEl.className = "git-file-dir";
      dirEl.textContent = file.path.slice(0, lastSlash + 1);

      const nameEl = document.createElement("span");
      nameEl.className = "git-file-name";
      nameEl.textContent = file.path.slice(lastSlash + 1);

      pathEl.appendChild(dirEl);
      pathEl.appendChild(nameEl);
    } else {
      const nameEl = document.createElement("span");
      nameEl.className = "git-file-name";
      nameEl.textContent = file.path;
      pathEl.appendChild(nameEl);
    }
    pathEl.addEventListener("click", () => this.viewDiff(file.path, isStaged));

    const actions = document.createElement("span");
    actions.className = "git-file-actions";

    if (isStaged) {
      actions.appendChild(
        this.createActionBtn("−", "Unstage file", "", () =>
          this.unstageFile(file.path),
        ),
      );
    } else {
      actions.appendChild(
        this.createActionBtn("+", "Stage file", "", () =>
          this.stageFile(file.path),
        ),
      );
      actions.appendChild(
        this.createActionBtn("✕", "Discard changes", "git-action-discard", () =>
          this.discardFile(file.path),
        ),
      );
    }

    row.appendChild(statusIcon);
    row.appendChild(pathEl);
    row.appendChild(actions);
    return row;
  }

  private renderCommitArea(): HTMLElement {
    const commitArea = document.createElement("div");
    commitArea.className = "git-commit-area";

    const textarea = document.createElement("textarea");
    textarea.className = "git-commit-input";
    textarea.placeholder = "Commit message (Ctrl+Enter to commit)";
    textarea.setAttribute("aria-label", "Commit message");
    textarea.rows = 2;
    textarea.style.minHeight = "40px";
    textarea.style.maxHeight = "150px";

    textarea.addEventListener("input", () => {
      textarea.style.height = "auto";
      textarea.style.height = `${Math.min(textarea.scrollHeight, 150)}px`;
      this.clearValidationError(textarea);
    });

    textarea.addEventListener("keydown", (e) => {
      if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
        e.preventDefault();
        this.doCommit(textarea);
      }
    });

    commitArea.appendChild(this.renderCommitPrefixes(textarea));
    commitArea.appendChild(textarea);

    const commitBtn = document.createElement("button");
    commitBtn.className = "git-btn git-commit-btn";
    commitBtn.textContent = "Commit";
    commitBtn.setAttribute("aria-label", "Commit changes");
    commitBtn.disabled = this.stagedFiles.length === 0;
    commitBtn.addEventListener("click", () => this.doCommit(textarea));

    commitArea.appendChild(commitBtn);
    return commitArea;
  }

  private renderCommitPrefixes(textarea: HTMLTextAreaElement): HTMLElement {
    const prefixes = ["feat", "fix", "refactor", "docs", "test", "chore"];
    const container = document.createElement("div");
    container.className = "git-commit-prefixes";

    for (const prefix of prefixes) {
      const btn = document.createElement("button");
      btn.className = "git-prefix-btn";
      if (this.commitPrefix === prefix) btn.classList.add("active");
      btn.textContent = prefix;
      btn.addEventListener("click", () => {
        this.applyPrefix(textarea, prefix);
        this.updatePrefixActiveStates(container, this.commitPrefix);
      });
      container.appendChild(btn);
    }

    return container;
  }

  private updatePrefixActiveStates(
    container: HTMLElement,
    activePrefix: string,
  ): void {
    for (const btn of container.querySelectorAll(".git-prefix-btn")) {
      btn.classList.toggle("active", btn.textContent === activePrefix);
    }
  }

  private applyPrefix(textarea: HTMLTextAreaElement, prefix: string): void {
    const prefixPattern = /^(feat|fix|refactor|docs|test|chore): /;

    if (this.commitPrefix === prefix) {
      this.commitPrefix = "";
      textarea.value = textarea.value.replace(prefixPattern, "");
      return;
    }

    this.commitPrefix = prefix;
    const newPrefix = `${prefix}: `;

    if (prefixPattern.test(textarea.value)) {
      textarea.value = textarea.value.replace(prefixPattern, newPrefix);
    } else {
      textarea.value = newPrefix + textarea.value;
    }
  }

  private createActionBtn(
    text: string,
    title: string,
    extraClass: string,
    onClick: () => void,
  ): HTMLButtonElement {
    const btn = document.createElement("button");
    btn.className = extraClass
      ? `git-action-btn ${extraClass}`
      : "git-action-btn";
    btn.textContent = text;
    btn.title = title;
    btn.setAttribute("aria-label", title);
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      onClick();
    });
    return btn;
  }

  private async stageFile(path: string): Promise<void> {
    try {
      await gitStage(this.workingDir, [path]);
      await this.refresh();
    } catch (err) {
      console.error("Stage failed:", err);
      this.onError?.(`Failed to stage file: ${String(err)}`);
    }
  }

  private async unstageFile(path: string): Promise<void> {
    try {
      await gitUnstage(this.workingDir, [path]);
      await this.refresh();
    } catch (err) {
      console.error("Unstage failed:", err);
      this.onError?.(`Failed to unstage file: ${String(err)}`);
    }
  }

  private async stageAll(paths: string[]): Promise<void> {
    if (paths.length === 0) return;
    try {
      await gitStage(this.workingDir, paths);
      await this.refresh();
    } catch (err) {
      console.error("Stage all failed:", err);
      this.onError?.(`Failed to stage files: ${String(err)}`);
    }
  }

  private async unstageAll(paths: string[]): Promise<void> {
    if (paths.length === 0) return;
    try {
      await gitUnstage(this.workingDir, paths);
      await this.refresh();
    } catch (err) {
      console.error("Unstage all failed:", err);
      this.onError?.(`Failed to unstage files: ${String(err)}`);
    }
  }

  private async discardFile(path: string): Promise<void> {
    if (
      !window.confirm(
        `Discard all changes to "${path}"? This cannot be undone.`,
      )
    )
      return;
    try {
      await gitDiscard(this.workingDir, [path]);
      await this.refresh();
    } catch (err) {
      console.error("Discard failed:", err);
      this.onError?.(`Failed to discard changes: ${String(err)}`);
    }
  }

  private async viewDiff(path: string, staged: boolean): Promise<void> {
    if (!this.onShowDiff) return;
    try {
      const diff = await gitDiff(this.workingDir, path, staged);
      let before: string | undefined;
      let after: string | undefined;

      try {
        before = await gitDiffBaseContent(this.workingDir, path, staged);
        const reconstructed = applyPatch(before, diff);
        if (reconstructed !== false) {
          after = reconstructed;
        } else {
          before = undefined;
        }
      } catch (reconstructErr) {
        console.warn(
          "Failed to reconstruct before/after from git diff:",
          reconstructErr,
        );
      }

      this.onShowDiff(diff, path, before, after);
    } catch (err) {
      console.error("Diff failed:", err);
      this.onError?.(`Failed to load diff: ${String(err)}`);
    }
  }

  private async doCommit(textarea: HTMLTextAreaElement): Promise<void> {
    this.clearValidationError(textarea);

    if (this.stagedFiles.length === 0) {
      this.showValidationError(
        textarea,
        "No staged files. Stage changes before committing.",
      );
      return;
    }

    const msg = textarea.value.trim();
    if (!msg) {
      this.showValidationError(textarea, "Commit message cannot be empty.");
      textarea.focus();
      return;
    }

    try {
      await gitCommit(this.workingDir, msg);
      textarea.value = "";
      this.commitPrefix = "";
      await this.refresh();
    } catch (err) {
      this.showValidationError(textarea, String(err));
    }
  }

  private showValidationError(
    textarea: HTMLTextAreaElement,
    message: string,
  ): void {
    this.clearValidationError(textarea);
    textarea.classList.add("git-commit-input-error");
    const errorEl = document.createElement("div");
    errorEl.className = "git-validation-error";
    errorEl.textContent = message;
    textarea.insertAdjacentElement("afterend", errorEl);
  }

  private clearValidationError(textarea: HTMLTextAreaElement): void {
    textarea.classList.remove("git-commit-input-error");
    const existing = textarea.parentElement?.querySelector(
      ".git-validation-error",
    );
    if (existing) existing.remove();
  }
}

function statusLetter(status: string): string {
  switch (status) {
    case "Modified":
      return "M";
    case "Added":
      return "A";
    case "Deleted":
      return "D";
    case "Renamed":
      return "R";
    case "Untracked":
      return "?";
    case "Conflicted":
      return "!";
    default:
      return "?";
  }
}
