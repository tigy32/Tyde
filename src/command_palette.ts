import type { FileEntry as BridgeFileEntry } from "@tyde/protocol";
import { escapeHtml } from "./renderer";

interface Command {
  id: string;
  label: string;
  shortcut?: string;
  execute: () => void;
}

interface FuzzyResult {
  match: boolean;
  score: number;
  indices: number[];
}

interface FileEntry {
  name: string;
  path: string;
  relativePath: string;
}

interface ScoredFile extends FileEntry {
  score: number;
  indices: number[];
}

const FILE_INDEX_MAX_DEPTH = 6;
const FILE_INDEX_MAX_FILES = 20_000;
const FILE_INDEX_YIELD_INTERVAL = 20;

export class CommandPalette {
  private static HISTORY_KEY = "tyde-command-history";

  private overlay: HTMLElement;
  private input: HTMLInputElement;
  private list: HTMLElement;
  private modeIndicator: HTMLElement;
  private commands: Command[] = [];
  private filteredCommands: Command[] = [];
  private selectedIndex: number = 0;
  private visible: boolean = false;
  private recentCommandIds: string[] = [];
  private showingSections: boolean = false;
  private recentCount: number = 0;

  private mode: "commands" | "files" = "commands";
  private fileEntries: FileEntry[] = [];
  private filteredFiles: ScoredFile[] = [];
  private workspaceRoot: string = "";
  private indexedRoot: string = "";
  private isIndexingFiles: boolean = false;
  private fileIndexToken: number = 0;

  onFileSelect: ((content: string, filePath: string) => void) | null = null;
  onError: ((message: string) => void) | null = null;

  constructor() {
    this.recentCommandIds = this.loadHistory();
    this.overlay = document.createElement("div");
    this.overlay.className = "command-palette-overlay";

    const container = document.createElement("div");
    container.className = "command-palette";

    this.input = document.createElement("input");
    this.input.className = "command-palette-input";
    this.input.type = "text";
    this.input.placeholder = "Search files... (type > for commands)";

    this.modeIndicator = document.createElement("div");
    this.modeIndicator.className = "command-palette-mode";

    this.list = document.createElement("div");
    this.list.className = "command-palette-list";

    container.appendChild(this.input);
    container.appendChild(this.modeIndicator);
    container.appendChild(this.list);
    this.overlay.appendChild(container);

    this.input.addEventListener("input", () => this.filter(this.input.value));

    this.input.addEventListener("keydown", (e) => {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        const count = this.activeItemCount();
        if (count === 0) return;
        this.selectedIndex = (this.selectedIndex + 1) % count;
        this.render();
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        const count = this.activeItemCount();
        if (count === 0) return;
        this.selectedIndex = (this.selectedIndex - 1 + count) % count;
        this.render();
        return;
      }
      if (e.key === "Enter") {
        e.preventDefault();
        this.executeSelected();
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        this.hide();
      }
    });

    // Dismiss on backdrop click — standard modal interaction, no dedicated close button needed
    this.overlay.addEventListener("click", (e) => {
      if (e.target === this.overlay) {
        this.hide();
      }
    });
  }

  registerCommand(command: Command): void {
    this.commands.push(command);
  }

  unregisterCommand(id: string): void {
    this.commands = this.commands.filter((c) => c.id !== id);
  }

  show(): void {
    this.visible = true;
    document.body.appendChild(this.overlay);
    this.input.value = "";
    this._matchIndices = new Map();
    this.mode = "files";
    this.updateModeIndicator();
    this.filteredFiles = this.fileEntries.map((f) => ({
      ...f,
      score: 0,
      indices: [],
    }));
    this.selectedIndex = 0;
    this.render();
    this.input.focus();
    this.ensureFileIndex();
  }

  hide(): void {
    this.visible = false;
    this.overlay.remove();
  }

  toggle(): void {
    if (this.visible) {
      this.hide();
    } else {
      this.show();
    }
  }

  isVisible(): boolean {
    return this.visible;
  }

  private filter(query: string): void {
    if (query.startsWith(">")) {
      this.mode = "commands";
      this.updateModeIndicator();
      const commandQuery = query.slice(1).trim();
      this.filterCommands(commandQuery);
      return;
    }

    this.mode = "files";
    this.updateModeIndicator();
    this.ensureFileIndex();
    this.filterFiles(query);
  }

  private filterCommands(query: string): void {
    if (!query) {
      this._matchIndices = new Map();
      this.buildSectionedList();
      this.selectedIndex = 0;
      this.render();
      return;
    }

    const scored: { command: Command; score: number; indices: number[] }[] = [];
    for (const command of this.commands) {
      const result = this.fuzzyMatch(query, command.label);
      if (!result.match) continue;
      scored.push({ command, score: result.score, indices: result.indices });
    }

    scored.sort((a, b) => b.score - a.score);
    this.showingSections = false;
    this.recentCount = 0;
    this.filteredCommands = scored.map((s) => s.command);
    this._matchIndices = new Map(scored.map((s) => [s.command, s.indices]));
    this.selectedIndex = 0;
    this.render();
  }

  private filterFiles(query: string): void {
    if (!query) {
      this.filteredFiles = this.fileEntries.map((f) => ({
        ...f,
        score: 0,
        indices: [],
      }));
      this.selectedIndex = 0;
      this.render();
      return;
    }

    const scored: ScoredFile[] = [];
    for (const file of this.fileEntries) {
      const result = this.fuzzyMatch(query, file.relativePath);
      if (!result.match) continue;
      scored.push({ ...file, score: result.score, indices: result.indices });
    }

    scored.sort((a, b) => b.score - a.score);
    this.filteredFiles = scored;
    this.selectedIndex = 0;
    this.render();
  }

  private _matchIndices: Map<Command, number[]> = new Map();

  private activeItemCount(): number {
    return this.mode === "files"
      ? this.filteredFiles.length
      : this.filteredCommands.length;
  }

  private render(): void {
    this.list.innerHTML = "";

    if (this.mode === "files") {
      this.renderFiles();
      return;
    }

    if (this.filteredCommands.length === 0) {
      const empty = document.createElement("div");
      empty.className = "command-palette-empty";
      empty.textContent = "No matching commands";
      this.list.appendChild(empty);
      return;
    }

    if (this.showingSections && this.recentCount > 0) {
      const recentHeader = document.createElement("div");
      recentHeader.className = "command-palette-section-header";
      recentHeader.textContent = "Recent";
      this.list.appendChild(recentHeader);
    }

    for (let i = 0; i < this.filteredCommands.length; i++) {
      if (this.showingSections && i === this.recentCount) {
        const allHeader = document.createElement("div");
        allHeader.className = "command-palette-section-header";
        allHeader.textContent = "All Commands";
        this.list.appendChild(allHeader);
      }

      const command = this.filteredCommands[i];
      const item = document.createElement("div");
      item.className =
        i === this.selectedIndex
          ? "command-palette-item selected"
          : "command-palette-item";

      const labelSpan = document.createElement("span");
      const indices = this._matchIndices.get(command);
      labelSpan.innerHTML = indices
        ? this.highlightLabel(command.label, indices)
        : escapeHtml(command.label);

      item.appendChild(labelSpan);

      if (command.shortcut) {
        const shortcutSpan = document.createElement("span");
        shortcutSpan.className = "command-palette-shortcut";
        shortcutSpan.textContent = command.shortcut;
        item.appendChild(shortcutSpan);
      }

      item.addEventListener("click", () => {
        this.recordHistory(command.id);
        command.execute();
        this.hide();
      });

      this.list.appendChild(item);
    }

    const selectedEl = this.list.querySelector(
      ".command-palette-item.selected",
    );
    if (selectedEl) selectedEl.scrollIntoView({ block: "nearest" });
  }

  private executeSelected(): void {
    if (this.mode === "files") {
      if (this.filteredFiles.length === 0) return;
      const file = this.filteredFiles[this.selectedIndex];
      this.openFile(file.path);
      this.hide();
      return;
    }
    if (this.filteredCommands.length === 0) return;
    const command = this.filteredCommands[this.selectedIndex];
    this.recordHistory(command.id);
    command.execute();
    this.hide();
  }

  private loadHistory(): string[] {
    const raw = localStorage.getItem(CommandPalette.HISTORY_KEY);
    if (!raw) return [];
    try {
      const parsed = JSON.parse(raw);
      if (Array.isArray(parsed)) return parsed;
      return [];
    } catch (err) {
      console.error(
        "Failed to load command palette history from localStorage:",
        err,
      );
      return [];
    }
  }

  private recordHistory(commandId: string): void {
    this.recentCommandIds = this.recentCommandIds.filter(
      (id) => id !== commandId,
    );
    this.recentCommandIds.unshift(commandId);
    this.recentCommandIds = this.recentCommandIds.slice(0, 5);
    localStorage.setItem(
      CommandPalette.HISTORY_KEY,
      JSON.stringify(this.recentCommandIds),
    );
  }

  private buildSectionedList(): void {
    const recentCommands: Command[] = [];
    const recentIdSet = new Set(this.recentCommandIds);

    for (const id of this.recentCommandIds) {
      const cmd = this.commands.find((c) => c.id === id);
      if (cmd) recentCommands.push(cmd);
    }

    const rest = this.commands.filter((c) => !recentIdSet.has(c.id));

    this.recentCount = recentCommands.length;
    this.showingSections = recentCommands.length > 0;
    this.filteredCommands = [...recentCommands, ...rest];
  }

  private highlightLabel(label: string, indices: number[]): string {
    const indexSet = new Set(indices);
    let result = "";
    for (let i = 0; i < label.length; i++) {
      const escaped = escapeHtml(label[i]);
      if (indexSet.has(i)) {
        result += `<strong class="command-palette-match">${escaped}</strong>`;
      } else {
        result += escaped;
      }
    }
    return result;
  }

  setWorkspaceRoot(root: string): void {
    if (root === this.workspaceRoot) return;
    this.workspaceRoot = root;
    this.indexedRoot = "";
    this.fileEntries = [];
    this.filteredFiles = [];
    this.isIndexingFiles = false;
    this.fileIndexToken++;
  }

  private ensureFileIndex(): void {
    if (!this.workspaceRoot) return;
    if (this.isIndexingFiles) return;
    if (this.indexedRoot === this.workspaceRoot && this.fileEntries.length > 0)
      return;
    void this.collectFilesIncremental();
  }

  private async collectFilesIncremental(): Promise<void> {
    if (!this.workspaceRoot) return;
    this.isIndexingFiles = true;
    const token = ++this.fileIndexToken;
    const skipDirs = [
      "node_modules",
      ".git",
      "target",
      "dist",
      "__pycache__",
      ".next",
    ];

    this.fileEntries = [];
    this.filteredFiles = [];
    if (this.visible && this.mode === "files") {
      this.render();
    }

    try {
      const { listDirectory } = await import("./bridge");
      const queue: Array<{ path: string; depth: number }> = [
        { path: this.workspaceRoot, depth: 0 },
      ];
      let processedDirs = 0;

      while (
        queue.length > 0 &&
        this.fileEntries.length < FILE_INDEX_MAX_FILES
      ) {
        if (token !== this.fileIndexToken) return;
        const current = queue.shift()!;
        if (current.depth > FILE_INDEX_MAX_DEPTH) continue;

        let entries: BridgeFileEntry[];
        try {
          entries = await listDirectory(current.path, false);
        } catch (err) {
          console.error(
            `Failed to list directory "${current.path}" during file indexing:`,
            err,
          );
          continue;
        }

        for (const entry of entries) {
          if (token !== this.fileIndexToken) return;
          if (entry.is_directory) {
            if (skipDirs.includes(entry.name)) continue;
            queue.push({ path: entry.path, depth: current.depth + 1 });
            continue;
          }

          const relativePath = entry.path.startsWith(this.workspaceRoot)
            ? entry.path.slice(this.workspaceRoot.length).replace(/^\//, "")
            : entry.path;
          this.fileEntries.push({
            name: entry.name,
            path: entry.path,
            relativePath,
          });
          if (this.fileEntries.length >= FILE_INDEX_MAX_FILES) break;
        }

        processedDirs++;
        if (processedDirs % FILE_INDEX_YIELD_INTERVAL === 0) {
          if (this.mode === "files") {
            this.filterFiles(
              this.input.value.startsWith(">") ? "" : this.input.value,
            );
          }
          await new Promise((resolve) => setTimeout(resolve, 0));
        }
      }
    } catch (err) {
      console.error("Failed to collect files for file indexing:", err);
    } finally {
      if (token === this.fileIndexToken) {
        this.indexedRoot = this.workspaceRoot;
        this.isIndexingFiles = false;
        if (this.mode === "files") {
          this.filterFiles(
            this.input.value.startsWith(">") ? "" : this.input.value,
          );
        }
      }
    }
  }

  private updateModeIndicator(): void {
    this.modeIndicator.textContent =
      this.mode === "commands" ? "⌘ Commands" : "📄 Files";
    this.modeIndicator.className = "command-palette-mode";
    if (this.mode === "files")
      this.modeIndicator.classList.add("command-palette-mode-files");
  }

  private async openFile(filePath: string): Promise<void> {
    if (!this.onFileSelect) return;
    try {
      const { readFileContent } = await import("./bridge");
      const result = await readFileContent(filePath);
      this.onFileSelect(result.content, result.path);
    } catch (err) {
      this.onError?.(`Failed to open file: ${String(err)}`);
    }
  }

  private renderFiles(): void {
    if (this.filteredFiles.length === 0) {
      const empty = document.createElement("div");
      empty.className = "command-palette-empty";
      if (this.isIndexingFiles) {
        empty.textContent = "Indexing files...";
      } else {
        empty.textContent = "No matching files";
      }
      this.list.appendChild(empty);
      return;
    }

    for (let i = 0; i < this.filteredFiles.length; i++) {
      const file = this.filteredFiles[i];
      const item = document.createElement("div");
      item.className =
        i === this.selectedIndex
          ? "command-palette-file-item selected"
          : "command-palette-file-item";

      const nameSpan = document.createElement("div");
      nameSpan.className = "command-palette-file-name";
      nameSpan.textContent = file.name;

      const pathSpan = document.createElement("div");
      pathSpan.className = "command-palette-file-path";
      if (file.indices.length > 0) {
        pathSpan.innerHTML = this.highlightLabel(
          file.relativePath,
          file.indices,
        );
      } else {
        pathSpan.textContent = file.relativePath;
      }

      item.appendChild(nameSpan);
      item.appendChild(pathSpan);

      item.addEventListener("click", () => {
        this.openFile(file.path);
        this.hide();
      });

      this.list.appendChild(item);
    }

    const selectedEl = this.list.querySelector(
      ".command-palette-file-item.selected",
    );
    if (selectedEl) selectedEl.scrollIntoView({ block: "nearest" });
  }

  private fuzzyMatch(query: string, text: string): FuzzyResult {
    const queryLower = query.toLowerCase();
    const textLower = text.toLowerCase();
    const indices: number[] = [];

    let textIdx = 0;
    for (let qi = 0; qi < queryLower.length; qi++) {
      const found = textLower.indexOf(queryLower[qi], textIdx);
      if (found === -1) return { match: false, score: 0, indices: [] };
      indices.push(found);
      textIdx = found + 1;
    }

    let score = 0;

    const substringPos = textLower.indexOf(queryLower);
    if (substringPos !== -1) {
      score += 100;
      if (substringPos === 0) score += 50;
    }

    const words = textLower.split(/[\s\-_]/);
    for (const word of words) {
      if (word.startsWith(queryLower)) {
        score += 50;
      }
    }

    for (let i = 0; i < indices.length; i++) {
      const pos = indices[i];

      if (i > 0 && pos === indices[i - 1] + 1) {
        score += 5;
      }

      if (
        pos === 0 ||
        text[pos - 1] === " " ||
        text[pos - 1] === "-" ||
        text[pos - 1] === "_"
      ) {
        score += 10;
      }

      if (i > 0) {
        const gap = pos - indices[i - 1] - 1;
        score -= gap;
      }
    }

    return { match: true, score, indices };
  }
}
