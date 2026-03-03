import { invoke } from "@tauri-apps/api/core";
import { listDirectory, readFileContent } from "./bridge";
import { escapeHtml } from "./renderer";
import type { FileEntry } from "./types";

const EXPLORER_BASE_INDENT_PX = 6;
const EXPLORER_NEST_INDENT_PX = 0;

export class FileExplorer {
  private container: HTMLElement;
  private rootPath = "";
  private expandedDirs = new Set<string>();
  private selectedRow: HTMLElement | null = null;
  private searchTerm = "";
  private contextMenu: HTMLElement | null = null;
  private showHidden = false;
  private readonly abortController = new AbortController();

  onFileSelect: ((content: string, path: string) => void) | null = null;
  onError: ((message: string) => void) | null = null;

  constructor(container: HTMLElement) {
    this.container = container;
    this.showHidden = localStorage.getItem("explorer-show-hidden") === "true";
    document.addEventListener("click", () => this.hideContextMenu(), {
      signal: this.abortController.signal,
    });
    document.addEventListener(
      "keydown",
      (e) => {
        if (e.key === "Escape") this.hideContextMenu();
      },
      { signal: this.abortController.signal },
    );
  }

  dispose(): void {
    this.abortController.abort();
  }

  setRootPath(path: string): void {
    this.rootPath = path;
    this.expandedDirs.clear();
    this.refresh();
  }

  async refresh(silent = false): Promise<void> {
    if (!this.rootPath) {
      this.container.innerHTML =
        '<div class="panel-empty-state"><span class="panel-empty-state-icon">📁</span><span>Select a workspace to browse files</span></div>';
      return;
    }

    if (!silent) {
      this.container.innerHTML =
        '<div class="panel-loading"><span class="loading-spinner"></span>Loading files...</div>';
    }

    try {
      const entries = await listDirectory(this.rootPath, this.showHidden);
      const fragment = document.createDocumentFragment();
      if (entries.length === 0) {
        fragment.appendChild(this.renderHeader());
        const empty = document.createElement("div");
        empty.className = "panel-empty-state";
        empty.innerHTML =
          '<span class="panel-empty-state-icon">📁</span><span>Empty directory</span>';
        fragment.appendChild(empty);
      } else {
        fragment.appendChild(this.renderHeader());
        fragment.appendChild(this.renderSearchBar());
        const tree = await this.renderEntries(entries, 0);
        fragment.appendChild(tree);
      }
      this.container.innerHTML = "";
      this.container.appendChild(fragment);
    } catch (err) {
      this.container.innerHTML = `<div class="explorer-error">${escapeHtml(String(err))}</div>`;
      this.onError?.(`Failed to load explorer: ${String(err)}`);
    }
  }

  private async renderEntries(
    entries: FileEntry[],
    depth: number,
  ): Promise<HTMLElement> {
    const list = document.createElement("div");
    list.className = "explorer-list";
    if (depth === 0) list.setAttribute("role", "tree");

    for (const entry of entries) {
      if (entry.is_directory) {
        this.appendDirEntry(list, entry, depth);
      } else {
        this.appendFileEntry(list, entry, depth);
      }
    }

    return list;
  }

  private appendDirEntry(
    list: HTMLElement,
    entry: FileEntry,
    depth: number,
  ): void {
    const row = document.createElement("div");
    row.className = "explorer-row";
    row.style.paddingLeft = this.indentPx(depth);
    row.setAttribute("role", "treeitem");

    const chevron = document.createElement("span");
    chevron.className = "explorer-icon explorer-dir-chevron";

    const folderGlyph = document.createElement("span");
    folderGlyph.className = "explorer-icon explorer-dir-glyph";

    const name = document.createElement("span");
    name.className = "explorer-name";
    name.textContent = entry.name;

    const isExpanded = this.expandedDirs.has(entry.path);
    chevron.textContent = isExpanded ? "▼" : "▶";
    folderGlyph.textContent = isExpanded ? "📂" : "📁";
    row.setAttribute("aria-expanded", String(isExpanded));
    if (isExpanded) row.classList.add("explorer-dir-expanded");

    row.appendChild(chevron);
    row.appendChild(folderGlyph);
    row.appendChild(name);
    list.appendChild(row);

    const childContainer = document.createElement("div");
    childContainer.className = "explorer-children";
    childContainer.setAttribute("role", "group");
    childContainer.dataset.path = entry.path;
    childContainer.style.display = isExpanded ? "block" : "none";

    if (isExpanded) {
      this.loadChildren(entry.path, childContainer, depth);
    }

    list.appendChild(childContainer);

    row.addEventListener("click", () =>
      this.toggleDir(entry.path, row, childContainer, depth),
    );
    row.addEventListener("contextmenu", (e) =>
      this.showContextMenu(e, entry.path, true),
    );
  }

  private appendFileEntry(
    list: HTMLElement,
    entry: FileEntry,
    depth: number,
  ): void {
    const row = document.createElement("div");
    row.className = "explorer-row";
    row.style.paddingLeft = this.indentPx(depth);
    row.setAttribute("role", "treeitem");

    const iconInfo = fileIcon(entry.name);
    const icon = document.createElement("span");
    icon.className = "explorer-icon explorer-file-icon";
    icon.classList.add(iconInfo.className);
    icon.textContent = iconInfo.label;

    const name = document.createElement("span");
    name.className = "explorer-name";
    name.textContent = entry.name;

    row.appendChild(icon);
    row.appendChild(name);

    if (entry.size !== null) {
      const sizeEl = document.createElement("span");
      sizeEl.className = "explorer-size";
      sizeEl.textContent = formatSize(entry.size);
      row.appendChild(sizeEl);
    }

    row.addEventListener("click", () => this.selectFile(entry.path, row));
    row.addEventListener("contextmenu", (e) =>
      this.showContextMenu(e, entry.path, false),
    );
    list.appendChild(row);
  }

  private async loadChildren(
    path: string,
    childContainer: HTMLElement,
    depth: number,
  ): Promise<void> {
    try {
      const children = await listDirectory(path, this.showHidden);
      if (children.length === 0) {
        const empty = document.createElement("div");
        empty.className = "explorer-dir-empty";
        empty.style.paddingLeft = this.indentPx(depth + 1);
        empty.textContent = "Empty";
        childContainer.appendChild(empty);
        return;
      }
      const childTree = await this.renderEntries(children, depth + 1);
      childContainer.appendChild(childTree);
    } catch (err) {
      childContainer.innerHTML = `<div class="explorer-error" style="padding-left: ${this.indentPx(depth + 1)}">${escapeHtml(String(err))}</div>`;
    }
  }

  private async toggleDir(
    path: string,
    row: HTMLElement,
    childContainer: HTMLElement,
    depth: number,
  ): Promise<void> {
    const chevron = row.querySelector(".explorer-dir-chevron") as HTMLElement;
    const folderGlyph = row.querySelector(".explorer-dir-glyph") as HTMLElement;

    if (this.expandedDirs.has(path)) {
      this.expandedDirs.delete(path);
      chevron.textContent = "▶";
      folderGlyph.textContent = "📁";
      row.setAttribute("aria-expanded", "false");
      row.classList.remove("explorer-dir-expanded");
      childContainer.classList.add("collapsing");
      setTimeout(() => {
        childContainer.style.display = "none";
        childContainer.classList.remove("collapsing");
      }, 150);
      return;
    }

    this.expandedDirs.add(path);
    chevron.textContent = "▼";
    folderGlyph.textContent = "📂";
    row.setAttribute("aria-expanded", "true");
    row.classList.add("explorer-dir-expanded");
    childContainer.innerHTML = "";

    const loader = document.createElement("div");
    loader.className = "explorer-dir-loading";
    loader.innerHTML = '<span class="loading-spinner"></span> Loading…';
    childContainer.appendChild(loader);
    childContainer.style.display = "block";

    try {
      const children = await listDirectory(path, this.showHidden);
      childContainer.innerHTML = "";
      if (children.length === 0) {
        const empty = document.createElement("div");
        empty.className = "explorer-dir-empty";
        empty.style.paddingLeft = this.indentPx(depth + 1);
        empty.textContent = "Empty";
        childContainer.appendChild(empty);
        childContainer.style.display = "block";
        return;
      }
      const childTree = await this.renderEntries(children, depth + 1);
      childContainer.appendChild(childTree);
      childContainer.style.display = "block";
    } catch (err) {
      childContainer.innerHTML = `<div class="explorer-error" style="padding-left: ${this.indentPx(depth + 1)}">${escapeHtml(String(err))}</div>`;
      childContainer.style.display = "block";
    }
  }

  private indentPx(depth: number): string {
    return `${depth * EXPLORER_NEST_INDENT_PX + EXPLORER_BASE_INDENT_PX}px`;
  }

  private renderSearchBar(): HTMLElement {
    const wrap = document.createElement("div");
    wrap.className = "explorer-search-wrap";

    const input = document.createElement("input");
    input.className = "explorer-search-input";
    input.type = "text";
    input.placeholder = "Filter files… (Ctrl+Shift+F)";
    input.setAttribute("aria-label", "Filter files");

    const clear = document.createElement("span");
    clear.className = "explorer-search-clear";
    clear.textContent = "✕";
    clear.style.display = "none";

    input.addEventListener("input", () => {
      this.searchTerm = input.value.toLowerCase();
      clear.style.display = input.value ? "" : "none";
      this.filterTree();
    });

    clear.addEventListener("click", () => {
      input.value = "";
      this.searchTerm = "";
      clear.style.display = "none";
      this.filterTree();
    });

    wrap.appendChild(input);
    wrap.appendChild(clear);
    return wrap;
  }

  private filterTree(): void {
    const term = this.searchTerm;
    const rows = this.container.querySelectorAll(".explorer-row");
    const childContainers =
      this.container.querySelectorAll(".explorer-children");

    if (!term) {
      rows.forEach((r) => ((r as HTMLElement).style.display = ""));
      childContainers.forEach((c) => {
        const el = c as HTMLElement;
        const path = el.dataset.path;
        el.style.display =
          path && this.expandedDirs.has(path) ? "block" : "none";
      });
      this.container
        .querySelectorAll(".explorer-search-match")
        .forEach((mark) => {
          const parent = mark.parentNode;
          if (!parent) return;
          parent.replaceChild(
            document.createTextNode(mark.textContent || ""),
            mark,
          );
          parent.normalize();
        });
      return;
    }

    rows.forEach((r) => ((r as HTMLElement).style.display = "none"));
    childContainers.forEach((c) => ((c as HTMLElement).style.display = "none"));

    rows.forEach((r) => {
      const nameEl = r.querySelector(".explorer-name");
      if (!nameEl) return;
      const name = nameEl.textContent || "";

      if (!name.toLowerCase().includes(term)) {
        if (nameEl.querySelector(".explorer-search-match"))
          nameEl.textContent = name;
        return;
      }

      (r as HTMLElement).style.display = "";

      const idx = name.toLowerCase().indexOf(term);
      nameEl.innerHTML =
        escapeHtml(name.substring(0, idx)) +
        '<mark class="explorer-search-match">' +
        escapeHtml(name.substring(idx, idx + term.length)) +
        "</mark>" +
        escapeHtml(name.substring(idx + term.length));

      let el: HTMLElement | null = r as HTMLElement;
      while (el && el !== this.container) {
        if (el.classList.contains("explorer-children"))
          el.style.display = "block";
        if (el.classList.contains("explorer-row")) el.style.display = "";
        el = el.parentElement;
      }
    });
  }

  private showContextMenu(e: MouseEvent, path: string, isDir: boolean): void {
    e.preventDefault();
    this.hideContextMenu();

    const menu = document.createElement("div");
    menu.className = "explorer-context-menu";

    const items: { label: string; action: () => void }[] = [
      {
        label: "Copy Path",
        action: () => navigator.clipboard.writeText(path),
      },
      {
        label: "Copy Relative Path",
        action: () => {
          const rel = path.startsWith(this.rootPath)
            ? path.slice(this.rootPath.length).replace(/^\//, "")
            : path;
          navigator.clipboard.writeText(rel);
        },
      },
    ];

    if (!isDir) {
      items.push({
        label: "Open in Diff Panel",
        action: () => {
          this.openFileInDiffPanel(path);
        },
      });
    }

    items.push({
      label: "Reveal in File Manager",
      action: () => {
        this.revealInFileManager(path, isDir);
      },
    });

    // Clipboard ops grouped before file-system ops for discoverability
    for (let i = 0; i < items.length; i++) {
      if (i === 2) {
        const sep = document.createElement("div");
        sep.className = "explorer-context-menu-separator";
        menu.appendChild(sep);
      }
      const item = document.createElement("div");
      item.className = "explorer-context-menu-item";
      item.textContent = items[i].label;
      const action = items[i].action;
      item.addEventListener("click", (ev) => {
        ev.stopPropagation();
        this.hideContextMenu();
        action();
      });
      menu.appendChild(item);
    }

    // Viewport edge detection
    const menuItemCount = items.length;
    const estimatedHeight = menuItemCount * 30 + 8;
    let left = e.clientX;
    let top = e.clientY;
    if (left + 200 > window.innerWidth) left = window.innerWidth - 200;
    if (top + estimatedHeight > window.innerHeight)
      top = e.clientY - estimatedHeight;

    menu.style.left = `${left}px`;
    menu.style.top = `${top}px`;

    document.body.appendChild(menu);
    this.contextMenu = menu;
  }

  private async openFileInDiffPanel(path: string): Promise<void> {
    if (!this.onFileSelect) return;
    try {
      const result = await readFileContent(path);
      this.onFileSelect(result.content, result.path);
    } catch (err) {
      console.error("Failed to open file:", err);
      this.onError?.(`Failed to open file: ${String(err)}`);
    }
  }

  private revealInFileManager(path: string, isDir: boolean): void {
    const dirPath = isDir ? path : path.substring(0, path.lastIndexOf("/"));
    invoke("plugin:opener|reveal_item_in_dir", { path }).catch((firstErr) => {
      console.error("reveal_item_in_dir failed:", firstErr);
      invoke("plugin:shell|open", { path: dirPath }).catch((err) => {
        console.error(err);
        this.onError?.(`Failed to reveal path: ${String(err)}`);
      });
    });
  }

  private renderHeader(): HTMLElement {
    const header = document.createElement("div");
    header.className = "explorer-header";

    const breadcrumb = this.renderBreadcrumb();
    header.appendChild(breadcrumb);

    const toggle = document.createElement("button");
    toggle.className = "explorer-toggle-hidden";
    if (this.showHidden) toggle.classList.add("explorer-toggle-active");
    toggle.textContent = this.showHidden ? "◉" : "○";
    toggle.title = this.showHidden ? "Hide hidden files" : "Show hidden files";
    toggle.setAttribute("aria-label", "Toggle hidden files");
    toggle.setAttribute("aria-pressed", String(this.showHidden));
    toggle.addEventListener("click", () => {
      this.showHidden = !this.showHidden;
      toggle.textContent = this.showHidden ? "◉" : "○";
      toggle.title = this.showHidden
        ? "Hide hidden files"
        : "Show hidden files";
      toggle.classList.toggle("explorer-toggle-active", this.showHidden);
      toggle.setAttribute("aria-pressed", String(this.showHidden));
      localStorage.setItem("explorer-show-hidden", String(this.showHidden));
      this.refresh();
    });

    header.appendChild(toggle);
    return header;
  }

  private renderBreadcrumb(): HTMLElement {
    const nav = document.createElement("div");
    nav.className = "explorer-breadcrumb";

    if (!this.rootPath) return nav;

    const parts = this.rootPath.split("/").filter(Boolean);

    const maxSegments = 3;
    const startIdx = Math.max(0, parts.length - maxSegments);

    if (startIdx > 0) {
      const ellipsis = document.createElement("span");
      ellipsis.className = "explorer-breadcrumb-ellipsis";
      ellipsis.textContent = "…";
      nav.appendChild(ellipsis);
    }

    for (let i = startIdx; i < parts.length; i++) {
      if (i > startIdx || startIdx > 0) {
        const sep = document.createElement("span");
        sep.className = "explorer-breadcrumb-sep";
        sep.textContent = "/";
        nav.appendChild(sep);
      }

      const segment = document.createElement("span");
      segment.className = "explorer-breadcrumb-segment";
      segment.textContent = parts[i];

      const segmentPath = `/${parts.slice(0, i + 1).join("/")}`;
      segment.title = segmentPath;

      segment.addEventListener("click", () => {
        this.rootPath = segmentPath;
        this.expandedDirs.clear();
        this.refresh();
      });

      if (i === parts.length - 1) {
        segment.classList.add("explorer-breadcrumb-current");
      }

      nav.appendChild(segment);
    }

    return nav;
  }

  private hideContextMenu(): void {
    if (!this.contextMenu) return;
    this.contextMenu.remove();
    this.contextMenu = null;
  }

  private async selectFile(path: string, row: HTMLElement): Promise<void> {
    if (this.selectedRow)
      this.selectedRow.classList.remove("explorer-row-selected");
    row.classList.add("explorer-row-selected");
    this.selectedRow = row;

    if (!this.onFileSelect) return;
    try {
      const result = await readFileContent(path);
      this.onFileSelect(result.content, result.path);
    } catch (err) {
      const existing = row.parentElement?.querySelector(".explorer-error");
      if (existing) existing.remove();
      const errEl = document.createElement("div");
      errEl.className = "explorer-error";
      errEl.textContent = `Failed to read: ${String(err)}`;
      row.insertAdjacentElement("afterend", errEl);
      this.onError?.(`Failed to read file: ${String(err)}`);
    }
  }
}

interface FileIconInfo {
  label: string;
  className: string;
}

function fileIcon(name: string): FileIconInfo {
  if (name.endsWith(".lock"))
    return { label: "🔒", className: "explorer-icon-lock" };

  const ext = name.split(".").pop()?.toLowerCase() ?? "";
  switch (ext) {
    case "ts":
    case "tsx":
      return { label: "TS", className: "explorer-icon-ts" };
    case "js":
    case "jsx":
      return { label: "JS", className: "explorer-icon-js" };
    case "rs":
      return { label: "rs", className: "explorer-icon-rs" };
    case "json":
      return { label: "{}", className: "explorer-icon-json" };
    case "css":
    case "scss":
      return { label: "#", className: "explorer-icon-css" };
    case "html":
      return { label: "<>", className: "explorer-icon-html" };
    case "md":
      return { label: "M", className: "explorer-icon-md" };
    case "py":
      return { label: "py", className: "explorer-icon-py" };
    case "toml":
    case "yaml":
    case "yml":
      return { label: "⚙", className: "explorer-icon-config" };
    case "svg":
    case "png":
    case "jpg":
    case "gif":
    case "ico":
    case "webp":
      return { label: "◻", className: "explorer-icon-image" };
    default:
      return { label: "·", className: "explorer-icon-default" };
  }
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}
