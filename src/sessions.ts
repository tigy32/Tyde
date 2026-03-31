import {
  elementScroll,
  observeElementOffset,
  observeElementRect,
  Virtualizer,
} from "@tanstack/virtual-core";
import { confirm } from "@tauri-apps/plugin-dialog";
import type { SessionMetadata, SessionRecord } from "@tyde/protocol";
import {
  type BackendKind,
  listSessionRecords,
  normalizeBackendKind,
  renameSession,
} from "./bridge";
import { escapeHtml } from "./renderer";
import { promptForText } from "./text_prompt";

interface NormalizedSession {
  key: string;
  id: string;
  tydeSessionId: string | null;
  backendKind: BackendKind;
  preview: string;
  createdAtMs: number;
  messageCount: number | null;
  workspaceRoot?: string;
  parentId: string | null;
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
  private records = new Map<string, SessionRecord>();
  private showAgentSessions = false;
  private showNonTydeSessions = false;
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
    void this.migrateLocalStorageAliases();
    this.render();
  }

  async refreshRecords(): Promise<void> {
    try {
      const records = await listSessionRecords();
      this.records.clear();
      for (const r of records) {
        this.records.set(r.id, r);
      }
    } catch (err) {
      console.error("Failed to fetch session records:", err);
    }
  }

  update(sessions: SessionMetadata[]): void {
    const normalized = sessions
      .map((s) => this.normalizeSession(s))
      .filter((s): s is NormalizedSession => s !== null);

    // Inject sub-agent sessions from store that aren't in the backend list
    const matchedRecordIds = new Set(
      normalized
        .map((s) => s.tydeSessionId)
        .filter((id): id is string => id !== null),
    );
    for (const record of this.records.values()) {
      if (matchedRecordIds.has(record.id)) continue;
      if (!record.parent_id) continue;
      normalized.push({
        key: `store:${record.id}`,
        id: record.backend_session_id ?? record.id,
        tydeSessionId: record.id,
        backendKind: normalizeBackendKind(record.backend_kind),
        preview: record.alias ?? "Sub-agent session",
        createdAtMs: record.created_at_ms,
        messageCount: record.message_count,
        workspaceRoot: record.workspace_root ?? undefined,
        parentId: record.parent_id,
      });
    }

    this.sessions = normalized;
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

  getResolvedAlias(tydeSessionId: string): string | undefined {
    const record = this.records.get(tydeSessionId);
    if (!record) return undefined;
    const alias = record.user_alias ?? record.alias;
    return alias && alias.trim().length > 0 ? alias.trim() : undefined;
  }

  getResolvedAliasForBackendSession(
    backendSessionId: string,
    backendKind: BackendKind,
  ): string | undefined {
    for (const record of this.records.values()) {
      if (
        record.backend_session_id === backendSessionId &&
        normalizeBackendKind(record.backend_kind) === backendKind
      ) {
        const alias = record.user_alias ?? record.alias;
        return alias && alias.trim().length > 0 ? alias.trim() : undefined;
      }
    }
    return undefined;
  }

  private applyFilter(): void {
    let base = this.sessions;
    // Exclude sessions with zero messages (created but never used)
    base = base.filter((s) => s.messageCount !== 0);
    // Hide sessions not tracked by Tyde unless toggle is on
    if (!this.showNonTydeSessions) {
      base = base.filter((s) => s.tydeSessionId !== null);
    }
    // Filter out agent sessions unless toggle is on
    if (!this.showAgentSessions) {
      base = base.filter((s) => s.parentId === null);
    }
    const q = this.searchQuery.toLowerCase();
    if (!q) {
      this.filteredSessions = base;
      return;
    }
    this.filteredSessions = base.filter((s) => {
      const title = this.resolveSessionTitle(s).toLowerCase();
      const preview = s.preview.toLowerCase();
      const workspace = (s.workspaceRoot ?? "").toLowerCase();
      const date = this.formatDate(s.createdAtMs).toLowerCase();
      const backend = s.backendKind.toLowerCase();
      return (
        title.includes(q) ||
        preview.includes(q) ||
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
    refreshBtn.textContent = "\u21BB";
    refreshBtn.title = "Refresh sessions";
    refreshBtn.addEventListener("click", () => this.onRefresh?.());

    const agentToggle = document.createElement("button");
    agentToggle.className = `sessions-action-btn sessions-agent-toggle${this.showAgentSessions ? " active" : ""}`;
    agentToggle.title = this.showAgentSessions
      ? "Hide sub-agent sessions"
      : "Show sub-agent sessions";
    agentToggle.textContent = "\u229F";
    agentToggle.addEventListener("click", () => {
      this.showAgentSessions = !this.showAgentSessions;
      agentToggle.classList.toggle("active", this.showAgentSessions);
      agentToggle.title = this.showAgentSessions
        ? "Hide sub-agent sessions"
        : "Show sub-agent sessions";
      this.applyFilter();
      this.updateVirtualList();
    });

    const externalToggle = document.createElement("button");
    externalToggle.className = `sessions-action-btn sessions-external-toggle${this.showNonTydeSessions ? " active" : ""}`;
    externalToggle.title = this.showNonTydeSessions
      ? "Hide sessions from outside Tyde"
      : "Show sessions from outside Tyde";
    externalToggle.textContent = "\u29C9";
    externalToggle.addEventListener("click", () => {
      this.showNonTydeSessions = !this.showNonTydeSessions;
      externalToggle.classList.toggle("active", this.showNonTydeSessions);
      externalToggle.title = this.showNonTydeSessions
        ? "Hide sessions from outside Tyde"
        : "Show sessions from outside Tyde";
      this.applyFilter();
      this.updateVirtualList();
    });

    toolbar.appendChild(newBtn);
    toolbar.appendChild(agentToggle);
    toolbar.appendChild(externalToggle);
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
      error.innerHTML = `<span class="sessions-error-icon">\u26A0</span><span>${escapeHtml(this.errorMessage)}</span>`;
      this.container.appendChild(error);
      return;
    }

    if (this.sessions.length === 0) {
      const empty = document.createElement("div");
      empty.className = "sessions-empty";
      empty.innerHTML =
        '<span class="sessions-empty-icon">\uD83D\uDCAC</span><span>No saved sessions yet</span><span class="sessions-empty-hint">Start a conversation to create a session</span>';
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
    renameBtn.textContent = "\u270E";
    renameBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      void this.renameSessionAlias(session);
    });
    actions.appendChild(renameBtn);

    const deleteBtn = document.createElement("button");
    deleteBtn.type = "button";
    deleteBtn.className = "session-card-action-btn session-card-action-delete";
    deleteBtn.title = "Delete session";
    deleteBtn.textContent = "\uD83D\uDDD1\uFE0E";
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
            : session.backendKind === "gemini"
              ? "Gemini"
              : "Tycode";
    meta.appendChild(backend);

    const dot0 = document.createElement("span");
    dot0.className = "session-card-separator";
    dot0.textContent = "\u00B7";
    meta.appendChild(dot0);

    const date = document.createElement("span");
    date.className = "session-card-date";
    date.textContent = this.formatDate(session.createdAtMs);
    meta.appendChild(date);

    if (session.workspaceRoot) {
      const dot = document.createElement("span");
      dot.className = "session-card-separator";
      dot.textContent = "\u00B7";
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
      dot2.textContent = "\u00B7";
      meta.appendChild(dot2);

      const count = document.createElement("span");
      count.className = "session-card-count";
      count.textContent = `${session.messageCount} msg${session.messageCount !== 1 ? "s" : ""}`;
      meta.appendChild(count);
    }

    const dot3 = document.createElement("span");
    dot3.className = "session-card-separator";
    dot3.textContent = "\u00B7";
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

    // Match with store record by backend_session_id + backend_kind
    let matchedRecord: SessionRecord | undefined;
    for (const record of this.records.values()) {
      if (
        record.backend_session_id === sessionId &&
        normalizeBackendKind(record.backend_kind) === backendKind
      ) {
        matchedRecord = record;
        break;
      }
    }

    // Use store message_count if available, otherwise fall back to backend
    const messageCountRaw = matchedRecord
      ? matchedRecord.message_count
      : this.asNumber(raw.message_count);

    // Use store created_at if available
    const finalCreatedAtMs = matchedRecord
      ? matchedRecord.created_at_ms
      : createdAtMs;

    return {
      key: this.sessionKey(sessionId, backendKind),
      id: sessionId,
      tydeSessionId: matchedRecord?.id ?? null,
      backendKind,
      preview,
      createdAtMs: finalCreatedAtMs,
      messageCount:
        messageCountRaw === null
          ? null
          : Math.max(0, Math.floor(messageCountRaw)),
      workspaceRoot:
        matchedRecord?.workspace_root ?? this.asString(raw.workspace_root),
      parentId: matchedRecord?.parent_id ?? null,
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
    return `\u2026${sep}${parts.slice(-2).join(sep)}`;
  }

  private truncate(text: string, max: number): string {
    const firstLine = text.split("\n")[0];
    if (firstLine.length <= max) return firstLine;
    return `${firstLine.slice(0, max - 1)}\u2026`;
  }

  private resolveSessionTitle(session: NormalizedSession): string {
    if (session.tydeSessionId) {
      const record = this.records.get(session.tydeSessionId);
      if (record) {
        const userAlias = record.user_alias?.trim();
        if (userAlias && userAlias.length > 0) {
          return this.truncate(userAlias, 80);
        }
        const alias = record.alias?.trim();
        if (alias && alias.length > 0) {
          return this.truncate(alias, 80);
        }
      }
    }
    return this.truncate(session.preview, 50);
  }

  private async renameSessionAlias(session: NormalizedSession): Promise<void> {
    const currentTitle = this.resolveSessionTitle(session);
    const next = await promptForText({
      title: "Session Title",
      defaultValue: currentTitle,
      placeholder: "Session title",
      confirmLabel: "Save",
    });
    if (next === null) return;
    const trimmed = next.trim();
    if (session.tydeSessionId) {
      await renameSession(session.tydeSessionId, trimmed);
      await this.refreshRecords();
    }
    this.renderVisibleCards();
  }

  private async migrateLocalStorageAliases(): Promise<void> {
    const raw = localStorage.getItem(SESSION_ALIAS_STORAGE_KEY);
    if (!raw) {
      await this.refreshRecords();
      return;
    }
    const parsed = JSON.parse(raw);
    if (
      typeof parsed !== "object" ||
      parsed === null ||
      Array.isArray(parsed)
    ) {
      await this.refreshRecords();
      return;
    }
    const aliases = parsed as Record<string, string>;
    await this.refreshRecords();
    const promises: Promise<void>[] = [];
    for (const [key, alias] of Object.entries(aliases)) {
      if (!alias || !alias.trim()) continue;
      // key format is "backendKind:sessionId"
      const colonIdx = key.indexOf(":");
      if (colonIdx === -1) continue;
      const bk = key.slice(0, colonIdx);
      const sid = key.slice(colonIdx + 1);
      // Find matching record
      for (const record of this.records.values()) {
        if (
          record.backend_session_id === sid &&
          normalizeBackendKind(record.backend_kind) === normalizeBackendKind(bk)
        ) {
          promises.push(renameSession(record.id, alias.trim()));
          break;
        }
      }
    }
    await Promise.all(promises);
    localStorage.removeItem(SESSION_ALIAS_STORAGE_KEY);
  }
}
