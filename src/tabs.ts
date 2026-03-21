import { logTabPerf, perfNow } from "./perf_debug";

export type TabKind = "chat" | "file";
export type FileTabView = "file" | "diff";

export interface TabState {
  id: string;
  kind: TabKind;
  title: string;
  hasUnread: boolean;
  isStreaming: boolean;
  conversationId: number | null;
  filePath: string | null;
  fileView: FileTabView | null;
}

export interface RuntimeTabState {
  tabs: TabState[];
  activeTabId: string | null;
  tabCounter: number;
}

type PendingDropTarget = { tabId: string; before: boolean } | "end" | null;
type RenameSource = "user" | "system";

export class TabManager {
  private tabs: TabState[] = [];
  private activeTabId: string | null = null;
  private tabCounter = 0;
  private tabBarEl: HTMLElement;
  private showNewTabButton = true;
  private contextMenuEl: HTMLElement | null = null;
  private draggingTabId: string | null = null;
  private pointerDragTabId: string | null = null;
  private pointerDragPointerId: number | null = null;
  private pointerDragStartX = 0;
  private pointerDragStartY = 0;
  private pointerDragActive = false;
  private pendingDropTarget: PendingDropTarget = null;
  private suppressNextClickForTabId: string | null = null;
  private pointerMoveHandler: ((e: PointerEvent) => void) | null = null;
  private pointerUpHandler: ((e: PointerEvent) => void) | null = null;
  private newTabIds = new Set<string>();
  private externalDragActive = false;
  private autoManagedChatTabIds = new Set<string>();
  private userRenamedChatTabIds = new Set<string>();

  onBeforeTabSwitch: (() => void) | null = null;
  onTabSwitch: ((tab: TabState) => void) | null = null;
  onTabClose: ((tab: TabState) => void) | null = null;
  onTabRenamed: ((tab: TabState) => void) | null = null;
  onNewTab: (() => void) | null = null;
  onExternalDragStart: ((tab: TabState) => void) | null = null;
  onExternalDragMove: ((clientX: number, clientY: number) => void) | null =
    null;
  onExternalDragEnd:
    | ((tab: TabState, clientX: number, clientY: number) => void)
    | null = null;
  constructor(tabBarEl: HTMLElement) {
    this.tabBarEl = tabBarEl;
    this.tabBarEl.setAttribute("role", "tablist");
    this.setupContextMenuDismiss();
    this.render();
  }

  createChatTab(
    conversationId: number | null = null,
    title?: string,
  ): TabState {
    if (conversationId !== null) {
      const existing = this.getTabByConversationId(conversationId);
      if (existing) {
        return existing;
      }
    }
    const tab = this.createBaseTab(
      "chat",
      title || `Chat ${this.tabCounter + 1}`,
    );
    tab.conversationId = conversationId;
    if (this.isDefaultChatTitle(tab.title)) {
      this.autoManagedChatTabIds.add(tab.id);
    }
    this.tabs.push(tab);
    this.render();

    return tab;
  }

  createFileTab(
    filePath: string,
    fileView: FileTabView,
    title?: string,
  ): TabState {
    const existing = this.getTabByFilePath(filePath, fileView);
    if (existing) {
      if (title) existing.title = title;
      this.render();
      return existing;
    }

    const tab = this.createBaseTab("file", title || this.basename(filePath));
    tab.filePath = filePath;
    tab.fileView = fileView;
    this.tabs.push(tab);
    this.render();
    return tab;
  }

  switchTo(tabId: string): void {
    if (tabId === this.activeTabId) return;
    const tab = this.tabs.find((t) => t.id === tabId);
    if (!tab) return;
    const start = perfNow();
    const beforeStart = perfNow();
    this.onBeforeTabSwitch?.();
    const beforeHookMs = perfNow() - beforeStart;
    this.activeTabId = tabId;
    tab.hasUnread = false;
    const renderStart = perfNow();
    this.render();
    const renderMs = perfNow() - renderStart;
    const callbackStart = perfNow();
    this.onTabSwitch?.(tab);
    const callbackMs = perfNow() - callbackStart;
    logTabPerf("TabManager.switchTo", perfNow() - start, {
      tabId,
      tabKind: tab.kind,
      beforeHookMs,
      renderMs,
      callbackMs,
    });
  }

  closeTab(tabId: string): void {
    const idx = this.tabs.findIndex((t) => t.id === tabId);
    if (idx === -1) return;

    const tab = this.tabs[idx];
    this.tabs.splice(idx, 1);
    this.clearChatTitleTracking(tab.id);
    this.onTabClose?.(tab);

    if (tabId === this.activeTabId) {
      if (this.tabs.length > 0) {
        const newIdx = Math.min(idx, this.tabs.length - 1);
        this.activeTabId = this.tabs[newIdx].id;
        this.tabs[newIdx].hasUnread = false;
        this.onTabSwitch?.(this.tabs[newIdx]);
      } else {
        this.activeTabId = null;
      }
    }

    this.render();
  }

  removeTab(tabId: string): void {
    const idx = this.tabs.findIndex((t) => t.id === tabId);
    if (idx === -1) return;

    const tab = this.tabs[idx];
    this.tabs.splice(idx, 1);
    this.clearChatTitleTracking(tab.id);

    if (tabId === this.activeTabId) {
      if (this.tabs.length > 0) {
        const newIdx = Math.min(idx, this.tabs.length - 1);
        this.activeTabId = this.tabs[newIdx].id;
        this.tabs[newIdx].hasUnread = false;
        this.onTabSwitch?.(this.tabs[newIdx]);
      } else {
        this.activeTabId = null;
      }
    }

    this.render();
  }

  closeOthers(tabId: string): void {
    const keep = this.tabs.find((t) => t.id === tabId);
    if (!keep) return;
    const closing = this.tabs.filter((t) => t.id !== tabId);
    const wasAlreadyActive = keep.id === this.activeTabId;
    this.tabs = [keep];
    this.activeTabId = keep.id;
    keep.hasUnread = false;
    for (const tab of closing) {
      this.clearChatTitleTracking(tab.id);
      this.onTabClose?.(tab);
    }
    this.render();

    if (!wasAlreadyActive) {
      this.onTabSwitch?.(keep);
    }
  }

  closeAll(): void {
    const closing = [...this.tabs];
    this.tabs = [];
    this.activeTabId = null;
    this.autoManagedChatTabIds.clear();
    this.userRenamedChatTabIds.clear();
    for (const tab of closing) {
      this.onTabClose?.(tab);
    }
    this.render();
  }

  getActiveTab(): TabState | null {
    if (!this.activeTabId) return null;
    return this.tabs.find((t) => t.id === this.activeTabId) || null;
  }

  getPreferredChatTab(): TabState | null {
    const active = this.getActiveTab();
    if (active?.kind === "chat") return active;
    return this.tabs.find((t) => t.kind === "chat") || null;
  }

  getPreferredFileTab(): TabState | null {
    const active = this.getActiveTab();
    if (active?.kind === "file") return active;
    return this.tabs.find((t) => t.kind === "file") || null;
  }

  getTabByConversationId(convId: number): TabState | null {
    return (
      this.tabs.find((t) => t.kind === "chat" && t.conversationId === convId) ||
      null
    );
  }

  getTabByFilePath(filePath: string, fileView?: FileTabView): TabState | null {
    return (
      this.tabs.find(
        (t) =>
          t.kind === "file" &&
          t.filePath === filePath &&
          (fileView === undefined || t.fileView === fileView),
      ) || null
    );
  }

  updateTabState(tabId: string, partial: Partial<TabState>): void {
    const tab = this.tabs.find((t) => t.id === tabId);
    if (!tab) return;
    Object.assign(tab, partial);
  }

  markUnread(tabId: string): void {
    if (tabId === this.activeTabId) return;
    const tab = this.tabs.find((t) => t.id === tabId);
    if (!tab) return;
    tab.hasUnread = true;
    this.render();
  }

  setStreaming(tabId: string, streaming: boolean): void {
    const tab = this.tabs.find((t) => t.id === tabId);
    if (!tab || tab.kind !== "chat") return;
    tab.isStreaming = streaming;
  }

  getTabs(): TabState[] {
    return [...this.tabs];
  }

  hasTabs(): boolean {
    return this.tabs.length > 0;
  }

  renameTab(
    tabId: string,
    newTitle: string,
    source: RenameSource = "user",
  ): void {
    const tab = this.tabs.find((t) => t.id === tabId);
    if (!tab) return;
    const normalized = newTitle.replace(/\s+/g, " ").trim();
    if (!normalized || normalized === tab.title) return;

    tab.title = normalized;
    if (tab.kind === "chat") {
      if (source === "user") {
        this.userRenamedChatTabIds.add(tab.id);
        this.autoManagedChatTabIds.delete(tab.id);
      } else {
        this.autoManagedChatTabIds.add(tab.id);
      }
    }
    this.render();

    if (source === "user") {
      this.onTabRenamed?.(tab);
    }
  }

  canAutoRenameChatTab(conversationId: number): boolean {
    const tab = this.getTabByConversationId(conversationId);
    if (!tab || tab.kind !== "chat") return false;
    if (this.userRenamedChatTabIds.has(tab.id)) return false;
    return (
      this.autoManagedChatTabIds.has(tab.id) ||
      this.isDefaultChatTitle(tab.title)
    );
  }

  autoRenameChatTab(
    conversationId: number,
    newTitle: string,
    source: RenameSource = "system",
  ): boolean {
    const tab = this.getTabByConversationId(conversationId);
    if (!tab || tab.kind !== "chat") return false;

    const normalized = newTitle.replace(/\s+/g, " ").trim();
    if (!normalized) return false;
    if (source === "system" && this.userRenamedChatTabIds.has(tab.id))
      return false;

    const canAutoRename = this.canAutoRenameChatTab(conversationId);
    if (!canAutoRename) return false;
    if (tab.title === normalized) return false;

    this.renameTab(tab.id, normalized, source);
    return true;
  }

  setShowNewTabButton(show: boolean): void {
    this.showNewTabButton = show;
    this.render();
  }

  exportRuntimeState(): RuntimeTabState {
    return {
      tabs: this.tabs.map((tab) => ({ ...tab })),
      activeTabId: this.activeTabId,
      tabCounter: this.tabCounter,
    };
  }

  importRuntimeState(state: RuntimeTabState | null): void {
    if (!state) {
      this.tabs = [];
      this.activeTabId = null;
      this.autoManagedChatTabIds.clear();
      this.userRenamedChatTabIds.clear();
      this.render();
      return;
    }

    this.tabs = state.tabs.map((tab) => ({ ...tab }));
    this.tabCounter = state.tabCounter;
    if (
      state.activeTabId &&
      this.tabs.some((tab) => tab.id === state.activeTabId)
    ) {
      this.activeTabId = state.activeTabId;
    } else {
      this.activeTabId = this.tabs[0]?.id ?? null;
    }

    this.rebuildChatTitleTracking();
    this.render();
    this.emitActiveTab();
  }

  private emitActiveTab(): void {
    const active = this.getActiveTab();
    if (!active) return;
    active.hasUnread = false;
    this.onTabSwitch?.(active);
  }

  private createBaseTab(kind: TabKind, title: string): TabState {
    this.tabCounter++;
    const id = `tab-${Date.now()}-${this.tabCounter}`;
    this.newTabIds.add(id);
    return {
      id,
      kind,
      title,
      hasUnread: false,
      isStreaming: false,
      conversationId: null,
      filePath: null,
      fileView: null,
    };
  }

  private basename(path: string): string {
    const parts = path.replace(/\\/g, "/").split("/");
    return parts[parts.length - 1] || path;
  }

  private isDefaultChatTitle(title: string): boolean {
    return /^(?:chat|bridge)(?:\s+\d+)?$/i.test(title.trim());
  }

  private rebuildChatTitleTracking(): void {
    this.autoManagedChatTabIds.clear();
    this.userRenamedChatTabIds.clear();
    for (const tab of this.tabs) {
      if (tab.kind !== "chat") continue;
      if (this.isDefaultChatTitle(tab.title)) {
        this.autoManagedChatTabIds.add(tab.id);
      }
    }
  }

  private clearChatTitleTracking(tabId: string): void {
    this.autoManagedChatTabIds.delete(tabId);
    this.userRenamedChatTabIds.delete(tabId);
  }

  private render(): void {
    this.tabBarEl.innerHTML = "";

    for (const tab of this.tabs) {
      const el = document.createElement("div");
      el.className = "conv-tab";
      el.dataset.testid = "conv-tab";
      if (tab.kind === "file") el.classList.add("conv-tab-file");
      if (tab.id === this.activeTabId) el.classList.add("conv-tab-active");
      if (this.newTabIds.has(tab.id)) {
        el.classList.add("conv-tab-entering");
        this.newTabIds.delete(tab.id);
      }
      el.dataset.tabId = tab.id;
      el.draggable = false;
      el.setAttribute("role", "tab");
      el.setAttribute("aria-selected", String(tab.id === this.activeTabId));

      const titleSpan = document.createElement("span");
      titleSpan.className = "conv-tab-title";
      titleSpan.dataset.testid = "conv-tab-title";
      titleSpan.textContent = tab.title;
      if (tab.kind === "file" && tab.filePath) {
        titleSpan.title = tab.filePath;
      }
      el.appendChild(titleSpan);

      if (tab.hasUnread) {
        const dot = document.createElement("span");
        dot.className = "conv-tab-unread";
        el.appendChild(dot);
      }

      const closeBtn = document.createElement("button");
      closeBtn.className = "conv-tab-close";
      closeBtn.textContent = "×";
      closeBtn.setAttribute("aria-label", "Close tab");
      closeBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        this.closeTab(tab.id);
      });
      el.appendChild(closeBtn);

      el.addEventListener("click", () => {
        if (this.suppressNextClickForTabId === tab.id) {
          this.suppressNextClickForTabId = null;
          return;
        }
        this.switchTo(tab.id);
      });

      el.addEventListener("auxclick", (e) => {
        if (e.button === 1) {
          e.preventDefault();
          this.closeTab(tab.id);
        }
      });

      el.addEventListener("dblclick", (e) => {
        e.preventDefault();
        this.startRename(tab.id);
      });

      el.addEventListener("contextmenu", (e) => {
        e.preventDefault();
        this.showContextMenu(tab.id, e.clientX, e.clientY);
      });

      el.addEventListener("pointerdown", (e) => {
        this.startPointerDrag(e, tab.id, el);
      });

      el.addEventListener("dragstart", (e) => {
        this.draggingTabId = tab.id;
        el.classList.add("conv-tab-dragging");
        if (e.dataTransfer) {
          e.dataTransfer.effectAllowed = "move";
          e.dataTransfer.setData("text/plain", tab.id);
        }
      });

      el.addEventListener("dragover", (e) => {
        if (!this.draggingTabId || this.draggingTabId === tab.id) return;
        e.preventDefault();
        const rect = el.getBoundingClientRect();
        const before = e.clientX < rect.left + rect.width / 2;
        el.classList.toggle("conv-tab-drop-before", before);
        el.classList.toggle("conv-tab-drop-after", !before);
      });

      el.addEventListener("dragleave", () => {
        el.classList.remove("conv-tab-drop-before", "conv-tab-drop-after");
      });

      el.addEventListener("drop", (e) => {
        if (!this.draggingTabId || this.draggingTabId === tab.id) return;
        e.preventDefault();
        const rect = el.getBoundingClientRect();
        const before = e.clientX < rect.left + rect.width / 2;
        this.moveTabBeforeTarget(this.draggingTabId, tab.id, before);
      });

      el.addEventListener("dragend", () => {
        this.draggingTabId = null;
        this.clearDragIndicators();
      });

      this.tabBarEl.appendChild(el);
    }

    if (this.showNewTabButton) {
      const newBtn = document.createElement("button");
      newBtn.className = "conv-tab-new";
      newBtn.textContent = "+";
      newBtn.setAttribute("aria-label", "New tab");
      newBtn.addEventListener("click", () => this.onNewTab?.());
      newBtn.addEventListener("dragover", (e) => {
        if (!this.draggingTabId) return;
        e.preventDefault();
        newBtn.classList.add("conv-tab-drop-after");
      });
      newBtn.addEventListener("dragleave", () => {
        newBtn.classList.remove("conv-tab-drop-after");
      });
      newBtn.addEventListener("drop", (e) => {
        if (!this.draggingTabId) return;
        e.preventDefault();
        this.moveTabToEnd(this.draggingTabId);
        newBtn.classList.remove("conv-tab-drop-after");
      });
      this.tabBarEl.appendChild(newBtn);
    }
  }

  private moveTabBeforeTarget(
    sourceId: string,
    targetId: string,
    before: boolean,
  ): void {
    if (sourceId === targetId) return;
    const sourceIdx = this.tabs.findIndex((t) => t.id === sourceId);
    if (sourceIdx === -1) return;

    const [moved] = this.tabs.splice(sourceIdx, 1);
    let targetIdx = this.tabs.findIndex((t) => t.id === targetId);
    if (targetIdx === -1) {
      this.tabs.push(moved);
    } else {
      if (!before) targetIdx += 1;
      if (targetIdx > this.tabs.length) targetIdx = this.tabs.length;
      this.tabs.splice(targetIdx, 0, moved);
    }

    this.render();
  }

  private moveTabToEnd(sourceId: string): void {
    const sourceIdx = this.tabs.findIndex((t) => t.id === sourceId);
    if (sourceIdx === -1 || sourceIdx === this.tabs.length - 1) return;
    const [moved] = this.tabs.splice(sourceIdx, 1);
    this.tabs.push(moved);
    this.render();
  }

  private clearDragIndicators(): void {
    this.tabBarEl.querySelectorAll(".conv-tab").forEach((el) => {
      el.classList.remove(
        "conv-tab-dragging",
        "conv-tab-drop-before",
        "conv-tab-drop-after",
      );
    });
    this.tabBarEl.querySelectorAll(".conv-tab-new").forEach((el) => {
      el.classList.remove("conv-tab-drop-after");
    });
  }

  private startPointerDrag(
    e: PointerEvent,
    tabId: string,
    tabEl: HTMLElement,
  ): void {
    if (e.button !== 0) return;
    const target = e.target as HTMLElement | null;
    if (target?.closest(".conv-tab-close")) return;
    if (target?.closest(".conv-tab-rename-input")) return;
    if (this.pointerDragTabId !== null) return;

    this.pointerDragTabId = tabId;
    this.pointerDragPointerId = e.pointerId;
    this.pointerDragStartX = e.clientX;
    this.pointerDragStartY = e.clientY;
    this.pointerDragActive = false;
    this.pendingDropTarget = null;

    this.pointerMoveHandler = (evt: PointerEvent) =>
      this.handlePointerMove(evt);
    this.pointerUpHandler = (evt: PointerEvent) => this.handlePointerUp(evt);
    window.addEventListener("pointermove", this.pointerMoveHandler);
    window.addEventListener("pointerup", this.pointerUpHandler);
    window.addEventListener("pointercancel", this.pointerUpHandler);

    try {
      tabEl.setPointerCapture(e.pointerId);
    } catch (err) {
      console.error("Failed to set pointer capture for tab drag:", err);
    }
  }

  private handlePointerMove(e: PointerEvent): void {
    if (this.pointerDragTabId === null) return;
    if (
      this.pointerDragPointerId !== null &&
      e.pointerId !== this.pointerDragPointerId
    )
      return;

    const dx = e.clientX - this.pointerDragStartX;
    const dy = e.clientY - this.pointerDragStartY;
    if (!this.pointerDragActive) {
      if (Math.hypot(dx, dy) < 6) return;
      this.pointerDragActive = true;
      this.draggingTabId = this.pointerDragTabId;
    }

    e.preventDefault();

    const tab = this.tabs.find((t) => t.id === this.pointerDragTabId);
    if (tab?.kind === "chat") {
      const barRect = this.tabBarEl.getBoundingClientRect();
      const outsideVertically =
        e.clientY < barRect.top - 40 || e.clientY > barRect.bottom + 40;
      const outsideHorizontally =
        e.clientX < barRect.left || e.clientX > barRect.right;

      if (
        (outsideVertically || outsideHorizontally) &&
        !this.externalDragActive
      ) {
        this.externalDragActive = true;
        this.onExternalDragStart?.(tab);
      }

      if (this.externalDragActive) {
        const insideBar =
          e.clientX >= barRect.left &&
          e.clientX <= barRect.right &&
          e.clientY >= barRect.top &&
          e.clientY <= barRect.bottom;

        if (insideBar) {
          this.externalDragActive = false;
          if (tab) this.onExternalDragEnd?.(tab, e.clientX, e.clientY);
          this.updatePointerDropTarget(e.clientX, e.clientY);
          return;
        }
        this.onExternalDragMove?.(e.clientX, e.clientY);
        return;
      }
    }

    this.updatePointerDropTarget(e.clientX, e.clientY);
  }

  private updatePointerDropTarget(clientX: number, clientY: number): void {
    if (!this.draggingTabId) return;

    this.clearDragIndicators();
    this.pendingDropTarget = null;

    const hovered = document.elementFromPoint(
      clientX,
      clientY,
    ) as HTMLElement | null;
    if (!hovered) {
      this.markDraggingTab();
      return;
    }

    const endBtn = hovered.closest(".conv-tab-new") as HTMLElement | null;
    if (endBtn) {
      endBtn.classList.add("conv-tab-drop-after");
      this.pendingDropTarget = "end";
      this.markDraggingTab();
      return;
    }

    const tabEl = hovered.closest(
      ".conv-tab[data-tab-id]",
    ) as HTMLElement | null;
    if (!tabEl) {
      this.markDraggingTab();
      return;
    }

    const targetId = tabEl.dataset.tabId;
    if (!targetId || targetId === this.draggingTabId) {
      this.markDraggingTab();
      return;
    }

    const rect = tabEl.getBoundingClientRect();
    const before = clientX < rect.left + rect.width / 2;
    tabEl.classList.toggle("conv-tab-drop-before", before);
    tabEl.classList.toggle("conv-tab-drop-after", !before);
    this.pendingDropTarget = { tabId: targetId, before };
    this.markDraggingTab();
  }

  private markDraggingTab(): void {
    if (!this.draggingTabId) return;
    const activeDragTab = this.tabBarEl.querySelector<HTMLElement>(
      `.conv-tab[data-tab-id="${this.draggingTabId}"]`,
    );
    activeDragTab?.classList.add("conv-tab-dragging");
  }

  private handlePointerUp(e: PointerEvent): void {
    if (this.pointerDragTabId === null) return;
    if (
      this.pointerDragPointerId !== null &&
      e.pointerId !== this.pointerDragPointerId
    )
      return;

    if (this.externalDragActive) {
      const tab = this.tabs.find((t) => t.id === this.pointerDragTabId);
      if (tab) {
        this.onExternalDragEnd?.(tab, e.clientX, e.clientY);
      }
      this.externalDragActive = false;
      this.cleanupPointerDragState();
      return;
    }

    const draggedTabId = this.draggingTabId ?? this.pointerDragTabId;
    const dragged = this.pointerDragActive && !!draggedTabId;
    const dropTarget = this.pendingDropTarget;

    this.cleanupPointerDragState();

    if (!dragged || !draggedTabId || !dropTarget) return;

    this.suppressNextClickForTabId = draggedTabId;
    if (dropTarget === "end") {
      this.moveTabToEnd(draggedTabId);
      return;
    }
    this.moveTabBeforeTarget(draggedTabId, dropTarget.tabId, dropTarget.before);
  }

  private cleanupPointerDragState(): void {
    if (this.pointerMoveHandler) {
      window.removeEventListener("pointermove", this.pointerMoveHandler);
      this.pointerMoveHandler = null;
    }
    if (this.pointerUpHandler) {
      window.removeEventListener("pointerup", this.pointerUpHandler);
      window.removeEventListener("pointercancel", this.pointerUpHandler);
      this.pointerUpHandler = null;
    }

    this.pointerDragTabId = null;
    this.pointerDragPointerId = null;
    this.pointerDragStartX = 0;
    this.pointerDragStartY = 0;
    this.pointerDragActive = false;
    this.externalDragActive = false;
    this.pendingDropTarget = null;
    this.draggingTabId = null;
    this.clearDragIndicators();
  }

  private showContextMenu(tabId: string, x: number, y: number): void {
    this.dismissContextMenu();

    const menu = document.createElement("div");
    menu.className = "conv-tab-context-menu";
    menu.style.left = `${x}px`;
    menu.style.top = `${y}px`;

    const renameItem = document.createElement("div");
    renameItem.className = "conv-tab-context-item";
    renameItem.textContent = "Rename";
    renameItem.addEventListener("click", () => {
      this.dismissContextMenu();
      this.startRename(tabId);
    });
    menu.appendChild(renameItem);

    const sep = document.createElement("div");
    sep.className = "conv-tab-context-separator";
    menu.appendChild(sep);

    const closeItem = document.createElement("div");
    closeItem.className = "conv-tab-context-item";
    closeItem.textContent = "Close";
    closeItem.addEventListener("click", () => {
      this.dismissContextMenu();
      this.closeTab(tabId);
    });
    menu.appendChild(closeItem);

    const closeOthersItem = document.createElement("div");
    closeOthersItem.className = "conv-tab-context-item";
    closeOthersItem.textContent = "Close Others";
    closeOthersItem.addEventListener("click", () => {
      this.dismissContextMenu();
      this.closeOthers(tabId);
    });
    menu.appendChild(closeOthersItem);

    const closeAllItem = document.createElement("div");
    closeAllItem.className = "conv-tab-context-item";
    closeAllItem.textContent = "Close All";
    closeAllItem.addEventListener("click", () => {
      this.dismissContextMenu();
      this.closeAll();
    });
    menu.appendChild(closeAllItem);

    document.body.appendChild(menu);
    this.contextMenuEl = menu;

    const rect = menu.getBoundingClientRect();
    if (rect.right > window.innerWidth) {
      menu.style.left = `${window.innerWidth - rect.width - 4}px`;
    }
    if (rect.bottom > window.innerHeight) {
      menu.style.top = `${window.innerHeight - rect.height - 4}px`;
    }
  }

  private dismissContextMenu(): void {
    if (!this.contextMenuEl) return;
    this.contextMenuEl.remove();
    this.contextMenuEl = null;
  }

  private setupContextMenuDismiss(): void {
    document.addEventListener("click", () => this.dismissContextMenu());
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape") this.dismissContextMenu();
    });
  }

  private startRename(tabId: string): void {
    const tabEl = this.tabBarEl.querySelector(`[data-tab-id="${tabId}"]`);
    if (!tabEl) return;
    const titleSpan = tabEl.querySelector(
      ".conv-tab-title",
    ) as HTMLElement | null;
    if (!titleSpan) return;

    const tab = this.tabs.find((t) => t.id === tabId);
    if (!tab) return;

    const input = document.createElement("input");
    input.type = "text";
    input.className = "conv-tab-rename-input";
    input.value = tab.title;

    let committed = false;
    const commit = () => {
      if (committed) return;
      committed = true;
      const newName = input.value.trim();
      if (newName && newName !== tab.title) {
        this.renameTab(tab.id, newName, "user");
        return;
      }
      this.render();
    };

    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        commit();
      }
      if (e.key === "Escape") {
        e.preventDefault();
        committed = true;
        this.render();
      }
    });

    input.addEventListener("blur", commit);

    titleSpan.replaceWith(input);
    input.focus();
    input.select();
  }
}
