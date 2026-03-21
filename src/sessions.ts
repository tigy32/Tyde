import {
  elementScroll,
  observeElementOffset,
  observeElementRect,
  Virtualizer,
} from "@tanstack/virtual-core";
import { confirm } from "@tauri-apps/plugin-dialog";
import type { SessionMetadata } from "@tyde/protocol";
import { type BackendKind, normalizeBackendKind } from "./bridge";
import { escapeHtml } from "./renderer";
import { promptForText } from "./text_prompt";

interface NormalizedSession {
  key: string;
  id: string;
  backendKind: BackendKind;
  preview: string;
  createdAtMs: number;
  messageCount: number | null;
  workspaceRoot?: string;
}

const SESSION_ALIAS_STORAGE_KEY = "tyde-session-aliases";
const ROW_HEIGHT = 68;

export class SessionsPanel {
  private container: HTMLElement;
  private sessions: NormalizedSession[] = [];
  private filteredSessions: NormalizedSession[] = [];
  private searchQuery = "";
  private activeSessionKey: string | null = null;
  private resumingSessionKey: string | null = null;
  private state: "loading" | "loaded" | "error" = "loaded";
  private errorMessage = "";
  private aliases: Record<string, string> = {};
  private newSessionEnabled = true;
  private newSessionDisabledReason = "New sessions are unavailable.";

  private wrapperEl: HTMLElement | null = null;
  private virtualizer: Virtualizer<HTMLElement, HTMLElement> | null = null;
  private teardownVirtualizer: (() => void) | null = null;

  onResumeSession:
    | ((sessionId: string, backendKind: BackendKind) => void)
    | null = null;
  onNewSession: (() => void) | null = null;
  onRefresh: (() => void) | null = null;
  onDeleteSession:
    | ((sessionId: string, backendKind: BackendKind) => void)
    | null = null;
  onExportSession:
    | ((sessionId: string, backendKind: BackendKind) => void)
    | null = null;

  constructor(container: HTMLElement) {
    this.container = container;
    this.container.className = "sessions-panel";
    this.container.dataset.testid = "sessions-panel";
    this.aliases = this.loadAliases();
    this.render();
  }

  update(sessions: SessionMetadata[]): void {
    this.sessions = sessions
      .map((s) => this.normalizeSession(s))
      .filter((s): s is NormalizedSession => s !== null);
    this.state = "loaded";
    this.errorMessage = "";
    this.resumingSessionKey = null;
    this.applyFilter();
    this.render();
  }

  showLoading(): void {
    this.state = "loading";
    this.render();
  }

  showError(message: string): void {
    this.state = "error";
    this.errorMessage = message;
    this.render();
  }

  setNewSessionAvailability(enabled: boolean, reason?: string | null): void {
    this.newSessionEnabled = enabled;
    if (typeof reason === "string" && reason.trim().length > 0) {
      this.newSessionDisabledReason = reason;
    }
    this.render();
  }

  setActiveSession(
    sessionId: string | null,
    backendKind: BackendKind = "tycode",
  ): void {
    this.activeSessionKey = sessionId
      ? this.sessionKey(sessionId, backendKind)
      : null;
    this.renderVisibleCards();
  }

  setResuming(
    sessionId: string | null,
    backendKind: BackendKind = "tycode",
  ): void {
    this.resumingSessionKey = sessionId
      ? this.sessionKey(sessionId, backendKind)
      : null;
    this.renderVisibleCards();
  }

  private applyFilter(): void {
    const q = this.searchQuery.toLowerCase();
    if (!q) {
      this.filteredSessions = this.sessions;
      return;
    }
    this.filteredSessions = this.sessions.filter((s) => {
      const preview = s.preview.toLowerCase();
      const alias = (this.aliases[s.key] ?? "").toLowerCase();
      const workspace = (s.workspaceRoot ?? "").toLowerCase();
      const date = this.formatDate(s.createdAtMs).toLowerCase();
      const backend = s.backendKind.toLowerCase();
      return (
        preview.includes(q) ||
        alias.includes(q) ||
        workspace.includes(q) ||
        date.includes(q) ||
        backend.includes(q)
      );
    });
  }

  private destroyVirtualizer(): void {
    if (this.teardownVirtualizer) {
      this.teardownVirtualizer();
      this.teardownVirtualizer = null;
    }
    this.virtualizer = null;
    this.wrapperEl = null;
  }

  private render(): void {
    this.destroyVirtualizer();
    this.container.innerHTML = "";

    const toolbar = document.createElement("div");
    toolbar.className = "sessions-toolbar";

    const newBtn = document.createElement("button");
    newBtn.className = "sessions-action-btn sessions-new-btn";
    newBtn.textContent = "+ New Session";
    newBtn.disabled = !this.newSessionEnabled;
    if (!this.newSessionEnabled) {
      newBtn.title = this.newSessionDisabledReason;
    }
    newBtn.addEventListener("click", () => {
      if (!this.newSessionEnabled) return;
      this.onNewSession?.();
    });

    const refreshBtn = document.createElement("button");
    refreshBtn.className = "sessions-action-btn sessions-refresh-btn";
    refreshBtn.dataset.testid = "sessions-refresh";
    refreshBtn.textContent = "↻";
    refreshBtn.title = "Refresh sessions";
    refreshBtn.addEventListener("click", () => this.onRefresh?.());

    toolbar.appendChild(newBtn);
    toolbar.appendChild(refreshBtn);
    this.container.appendChild(toolbar);

    if (this.state === "loading") {
      const loading = document.createElement("div");
      loading.className = "sessions-loading";
      loading.setAttribute("role", "list");
      loading.setAttribute("aria-busy", "true");
      loading.innerHTML =
        '<div class="loading-spinner"></div><span>Loading sessions...</span>';
      this.container.appendChild(loading);
      return;
    }

    if (this.state === "error") {
      const error = document.createElement("div");
      error.className = "sessions-error-state";
      error.innerHTML = `<span class="sessions-error-icon">⚠</span><span>${escapeHtml(this.errorMessage)}</span>`;
      this.container.appendChild(error);
      return;
    }

    if (this.sessions.length === 0) {
      const empty = document.createElement("div");
      empty.className = "sessions-empty";
      empty.innerHTML =
        '<span class="sessions-empty-icon">💬</span><span>No saved sessions yet</span><span class="sessions-empty-hint">Start a conversation to create a session</span>';
      this.container.appendChild(empty);
      return;
    }

    const searchWrap = document.createElement("div");
    searchWrap.className = "sessions-search-wrap";
    const searchInput = document.createElement("input");
    searchInput.className = "sessions-search";
    searchInput.type = "text";
    searchInput.placeholder = "Search sessions...";
    searchInput.setAttribute("aria-label", "Search sessions");
    searchInput.value = this.searchQuery;
    searchInput.addEventListener("input", () => {
      this.searchQuery = searchInput.value;
      this.applyFilter();
      this.updateVirtualList();
    });
    searchWrap.appendChild(searchInput);
    this.container.appendChild(searchWrap);

    const list = document.createElement("div");
    list.className = "sessions-list";
    list.dataset.testid = "sessions-list";
    list.setAttribute("role", "list");
    list.setAttribute("aria-busy", "false");

    const wrapper = document.createElement("div");
    wrapper.className = "sessions-list-wrapper";
    list.appendChild(wrapper);

    this.wrapperEl = wrapper;
    this.container.appendChild(list);

    const virtualizer = new Virtualizer<HTMLElement, HTMLElement>({
      count: this.filteredSessions.length,
      getScrollElement: () => list,
      estimateSize: () => ROW_HEIGHT,
      overscan: 5,
      gap: 6,
      scrollToFn: elementScroll,
      observeElementRect,
      observeElementOffset,
      onChange: () => this.renderVisibleCards(),
    });

    this.virtualizer = virtualizer;
    this.teardownVirtualizer = virtualizer._didMount();
    virtualizer._willUpdate();
    this.renderVisibleCards();
  }

  private updateVirtualList(): void {
    if (!this.virtualizer) return;
    this.virtualizer.setOptions({
      ...this.virtualizer.options,
      count: this.filteredSessions.length,
    });
    this.virtualizer._willUpdate();
    this.renderVisibleCards();
  }

  private renderVisibleCards(): void {
    const wrapper = this.wrapperEl;
    const virtualizer = this.virtualizer;
    if (!wrapper || !virtualizer) return;

    virtualizer._willUpdate();
    const count = this.filteredSessions.length;

    if (count === 0) {
      if (this.searchQuery) {
        const noMatch = document.createElement("div");
        noMatch.className = "sessions-no-match";
        noMatch.textContent = "No sessions match your search";
        wrapper.replaceChildren(noMatch);
      } else {
        wrapper.replaceChildren();
      }
      return;
    }

    const virtualItems = virtualizer.getVirtualItems();
    if (virtualItems.length === 0) {
      const spacer = this.createSpacer(virtualizer.getTotalSize());
      wrapper.replaceChildren(spacer);
      return;
    }

    const frag = document.createDocumentFragment();
    const totalSize = virtualizer.getTotalSize();
    const first = virtualItems[0];
    const last = virtualItems[virtualItems.length - 1];

    const topSpacerSize = Math.max(0, first.start);
    if (topSpacerSize > 0) {
      frag.appendChild(this.createSpacer(topSpacerSize));
    }

    for (const item of virtualItems) {
      const card = this.createSessionCard(this.filteredSessions[item.index]);
      frag.appendChild(card);
    }

    const bottomSpacerSize = Math.max(0, totalSize - last.end);
    if (bottomSpacerSize > 0) {
      frag.appendChild(this.createSpacer(bottomSpacerSize));
    }

    wrapper.replaceChildren(frag);
  }

  private createSpacer(heightPx: number): HTMLElement {
    const spacer = document.createElement("div");
    spacer.style.height = `${heightPx}px`;
    spacer.setAttribute("aria-hidden", "true");
    return spacer;
  }

  private createSessionCard(session: NormalizedSession): HTMLElement {
    const card = document.createElement("div");
    const isActive = session.key === this.activeSessionKey;
    const isResuming = session.key === this.resumingSessionKey;
    card.className = `session-card${isActive ? " session-card-active" : ""}${isResuming ? " session-card-resuming" : ""}`;
    card.dataset.testid = "session-card";
    card.setAttribute("role", "listitem");
    card.setAttribute("aria-label", this.truncate(session.preview, 80));
    if (isActive) card.setAttribute("aria-current", "true");

    card.addEventListener("click", () => {
      if (isResuming) return;
      this.resumingSessionKey = session.key;
      this.renderVisibleCards();
      this.onResumeSession?.(session.id, session.backendKind);
    });

    const titleRow = document.createElement("div");
    titleRow.className = "session-card-title-row";

    const title = document.createElement("div");
    title.className = "session-card-title";
    title.textContent = this.resolveSessionTitle(session);
    titleRow.appendChild(title);

    const actions = document.createElement("div");
    actions.className = "session-card-actions";

    const renameBtn = document.createElement("button");
    renameBtn.type = "button";
    renameBtn.className = "session-card-action-btn";
    renameBtn.title = "Rename session";
    renameBtn.textContent = "✎";
    renameBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      void this.renameSessionAlias(session);
    });
    actions.appendChild(renameBtn);

    const deleteBtn = document.createElement("button");
    deleteBtn.type = "button";
    deleteBtn.className = "session-card-action-btn session-card-action-delete";
    deleteBtn.title = "Delete session";
    deleteBtn.textContent = "🗑\uFE0E";
    deleteBtn.addEventListener("click", async (e) => {
      e.stopPropagation();
      const confirmed = await confirm(
        `Delete session "${this.resolveSessionTitle(session)}"?`,
        { title: "Delete session", kind: "warning" },
      );
      if (!confirmed) return;
      this.onDeleteSession?.(session.id, session.backendKind);
    });
    actions.appendChild(deleteBtn);

    titleRow.appendChild(actions);
    card.appendChild(titleRow);

    const meta = document.createElement("div");
    meta.className = "session-card-meta";

    const backend = document.createElement("span");
    backend.className = `session-card-backend session-card-backend-${session.backendKind}`;
    backend.textContent =
      session.backendKind === "codex"
        ? "Codex"
        : session.backendKind === "claude"
          ? "Claude"
          : session.backendKind === "kiro"
            ? "Kiro"
            : "Tycode";
    meta.appendChild(backend);

    const dot0 = document.createElement("span");
    dot0.className = "session-card-separator";
    dot0.textContent = "·";
    meta.appendChild(dot0);

    const date = document.createElement("span");
    date.className = "session-card-date";
    date.textContent = this.formatDate(session.createdAtMs);
    meta.appendChild(date);

    if (session.workspaceRoot) {
      const dot = document.createElement("span");
      dot.className = "session-card-separator";
      dot.textContent = "·";
      meta.appendChild(dot);

      const workspace = document.createElement("span");
      workspace.className = "session-card-workspace";
      workspace.textContent = this.abbreviatePath(session.workspaceRoot);
      workspace.title = session.workspaceRoot;
      meta.appendChild(workspace);
    }

    if (session.messageCount !== null) {
      const dot2 = document.createElement("span");
      dot2.className = "session-card-separator";
      dot2.textContent = "·";
      meta.appendChild(dot2);

      const count = document.createElement("span");
      count.className = "session-card-count";
      count.textContent = `${session.messageCount} msg${session.messageCount !== 1 ? "s" : ""}`;
      meta.appendChild(count);
    }

    const dot3 = document.createElement("span");
    dot3.className = "session-card-separator";
    dot3.textContent = "·";
    meta.appendChild(dot3);

    const idLabel = document.createElement("span");
    idLabel.className = "session-card-id";
    idLabel.textContent = session.id.slice(0, 8);
    idLabel.title = session.id;
    meta.appendChild(idLabel);

    card.appendChild(meta);

    if (isResuming) {
      const overlay = document.createElement("div");
      overlay.className = "session-card-loading";
      overlay.innerHTML = '<div class="loading-spinner"></div>';
      card.appendChild(overlay);
    }

    return card;
  }

  private formatDate(epochMs: number): string {
    if (!Number.isFinite(epochMs)) return "Unknown";
    const date = new Date(epochMs);
    if (Number.isNaN(date.getTime())) return "Unknown";
    const months = [
      "Jan",
      "Feb",
      "Mar",
      "Apr",
      "May",
      "Jun",
      "Jul",
      "Aug",
      "Sep",
      "Oct",
      "Nov",
      "Dec",
    ];
    return `${months[date.getMonth()]} ${date.getDate()}, ${date.getFullYear()}`;
  }

  private normalizeSession(raw: SessionMetadata): NormalizedSession | null {
    const sessionId = this.asString(raw.session_id) ?? this.asString(raw.id);
    if (!sessionId) return null;
    const backendKind = normalizeBackendKind(raw.backend_kind);

    const preview =
      this.asString(raw.last_message_preview) ??
      this.asString(raw.preview) ??
      this.asString(raw.title) ??
      "New Session";

    const createdValue =
      this.asNumber(raw.created_at) ??
      this.asNumber(raw.last_modified) ??
      Date.now();
    const createdAtMs =
      createdValue > 1_000_000_000_000 ? createdValue : createdValue * 1000;

    const messageCountRaw = this.asNumber(raw.message_count);

    return {
      key: this.sessionKey(sessionId, backendKind),
      id: sessionId,
      backendKind,
      preview,
      createdAtMs,
      messageCount:
        messageCountRaw === null
          ? null
          : Math.max(0, Math.floor(messageCountRaw)),
      workspaceRoot: this.asString(raw.workspace_root),
    };
  }

  private asString(value: unknown): string | undefined {
    if (typeof value !== "string") return undefined;
    const trimmed = value.trim();
    return trimmed ? trimmed : undefined;
  }

  private asNumber(value: unknown): number | null {
    if (typeof value !== "number" || !Number.isFinite(value)) return null;
    return value;
  }

  private sessionKey(sessionId: string, backendKind: BackendKind): string {
    return `${backendKind}:${sessionId}`;
  }

  private abbreviatePath(path: string): string {
    const sep = path.includes("/") ? "/" : "\\";
    const parts = path.split(sep).filter(Boolean);
    if (parts.length <= 2) return path;
    return `…${sep}${parts.slice(-2).join(sep)}`;
  }

  private truncate(text: string, max: number): string {
    const firstLine = text.split("\n")[0];
    if (firstLine.length <= max) return firstLine;
    return `${firstLine.slice(0, max - 1)}…`;
  }

  private resolveSessionTitle(session: NormalizedSession): string {
    const alias = this.aliases[session.key];
    if (alias && alias.trim().length > 0) {
      return this.truncate(alias.trim(), 80);
    }
    return this.truncate(session.preview, 80);
  }

  private async renameSessionAlias(session: NormalizedSession): Promise<void> {
    const current = this.aliases[session.key] ?? "";
    const next = await promptForText({
      title: "Session Title",
      defaultValue: current || this.truncate(session.preview, 80),
      placeholder: "Session title",
      confirmLabel: "Save",
    });
    if (next === null) return;
    const trimmed = next.trim();
    if (!trimmed) {
      delete this.aliases[session.key];
    } else {
      this.aliases[session.key] = trimmed;
    }
    this.saveAliases();
    this.renderVisibleCards();
  }

  setSessionAlias(
    sessionId: string,
    backendKind: BackendKind,
    title: string,
  ): void {
    const key = this.sessionKey(sessionId, backendKind);
    const trimmed = title.trim();
    if (!trimmed) {
      delete this.aliases[key];
    } else {
      this.aliases[key] = trimmed;
    }
    this.saveAliases();
    this.renderVisibleCards();
  }

  getSessionAlias(
    sessionId: string,
    backendKind: BackendKind,
  ): string | undefined {
    const key = this.sessionKey(sessionId, backendKind);
    const alias = this.aliases[key];
    return alias && alias.trim().length > 0 ? alias.trim() : undefined;
  }

  private loadAliases(): Record<string, string> {
    try {
      const raw = localStorage.getItem(SESSION_ALIAS_STORAGE_KEY);
      if (!raw) return {};
      const parsed = JSON.parse(raw);
      if (
        typeof parsed !== "object" ||
        parsed === null ||
        Array.isArray(parsed)
      )
        return {};
      return parsed as Record<string, string>;
    } catch (err) {
      console.error("Failed to load session aliases from localStorage:", err);
      return {};
    }
  }

  private saveAliases(): void {
    try {
      localStorage.setItem(
        SESSION_ALIAS_STORAGE_KEY,
        JSON.stringify(this.aliases),
      );
    } catch (err) {
      console.error("Failed to save session aliases to localStorage:", err);
    }
  }
}
