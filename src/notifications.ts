export type NotificationType = "info" | "warning" | "error" | "success";

export interface NotificationAction {
  label: string;
  callback: () => void;
}

export interface NotificationOptions {
  type?: NotificationType;
  message: string;
  timeout?: number;
  actions?: NotificationAction[];
  persistent?: boolean;
}

interface NotificationItem {
  id: string;
  type: NotificationType;
  message: string;
  actions: NotificationAction[];
  persistent: boolean;
  timestamp: Date;
  element: HTMLElement | null;
  dismissTimer: number | null;
}

const TYPE_ICONS: Record<NotificationType, string> = {
  info: "ℹ",
  warning: "⚠",
  error: "✕",
  success: "✓",
};

const MAX_VISIBLE = 5;
const MAX_HISTORY = 50;
const DEFAULT_TIMEOUT = 5000;
const ERROR_TIMEOUT = 10000;

export class NotificationManager {
  private enabled: boolean = true;
  private soundEnabled: boolean = false;
  private container: HTMLElement | null = null;
  private historyPanelEl: HTMLElement | null = null;
  private idCounter = 0;
  private badgeIntervalId: ReturnType<typeof setInterval> | null = null;

  private visible: NotificationItem[] = [];
  private queue: NotificationItem[] = [];
  private history: NotificationItem[] = [];
  private unreadCount = 0;

  async requestPermission(): Promise<void> {
    if (!("Notification" in window)) return;
    if (Notification.permission === "granted") return;
    await Notification.requestPermission();
  }

  notifyUserInputNeeded(question: string): void {
    this.show({ type: "warning", message: `Input Needed: ${question}` });
    this.systemNotify("Input Needed", question);
  }

  notifyTaskComplete(title: string): void {
    this.show({ type: "success", message: title });
    this.systemNotify("Task Complete", title);
  }

  notifyError(message: string): void {
    this.show({ type: "error", message });
    this.systemNotify("Error", message);
  }

  setEnabled(enabled: boolean): void {
    this.enabled = enabled;
  }

  setSoundEnabled(enabled: boolean): void {
    this.soundEnabled = enabled;
  }

  show(options: NotificationOptions): string {
    const type = options.type ?? "info";
    const id = `notif-${++this.idCounter}`;

    const item: NotificationItem = {
      id,
      type,
      message: options.message,
      actions: options.actions ?? [],
      persistent: options.persistent ?? false,
      timestamp: new Date(),
      element: null,
      dismissTimer: null,
    };

    this.addToHistory(item);
    this.unreadCount++;

    if (!this.enabled) return id;

    if (type === "error" && this.soundEnabled) {
      this.playErrorSound();
    }

    if (this.visible.length >= MAX_VISIBLE) {
      this.queue.push(item);
      return id;
    }

    this.presentToast(item, options);
    return id;
  }

  dismiss(id: string): void {
    const visibleIdx = this.visible.findIndex((n) => n.id === id);
    if (visibleIdx !== -1) {
      this.removeToast(this.visible[visibleIdx]);
      return;
    }
    const queueIdx = this.queue.findIndex((n) => n.id === id);
    if (queueIdx !== -1) {
      this.queue.splice(queueIdx, 1);
    }
  }

  info(message: string, actions?: NotificationAction[]): string {
    return this.show({ type: "info", message, actions });
  }

  warning(message: string, actions?: NotificationAction[]): string {
    return this.show({ type: "warning", message, actions });
  }

  error(message: string, actions?: NotificationAction[]): string {
    return this.show({ type: "error", message, actions });
  }

  success(message: string, actions?: NotificationAction[]): string {
    return this.show({ type: "success", message, actions });
  }

  getHistory(): NotificationItem[] {
    return [...this.history];
  }

  getUnreadCount(): number {
    return this.unreadCount;
  }

  clearHistory(): void {
    this.history = [];
    this.unreadCount = 0;
  }

  markAllRead(): void {
    this.unreadCount = 0;
  }

  createBellButton(): HTMLElement {
    const btn = document.createElement("button");
    btn.className = "notification-bell-btn";
    btn.setAttribute("aria-label", "Notifications");
    btn.setAttribute("aria-expanded", "false");

    const icon = document.createElement("span");
    icon.className = "notification-bell-icon";
    icon.setAttribute("aria-hidden", "true");
    icon.textContent = "◌";
    btn.appendChild(icon);

    const badge = document.createElement("span");
    badge.className = "notification-badge";
    badge.style.display = "none";
    btn.appendChild(badge);

    const updateBadge = () => {
      if (this.unreadCount > 0) {
        badge.textContent = String(this.unreadCount);
        badge.setAttribute(
          "aria-label",
          `${this.unreadCount} unread notifications`,
        );
        badge.style.display = "";
      } else {
        badge.style.display = "none";
      }
    };

    if (this.badgeIntervalId !== null) clearInterval(this.badgeIntervalId);
    this.badgeIntervalId = setInterval(updateBadge, 1000);
    updateBadge();

    btn.addEventListener("click", () => {
      this.markAllRead();
      updateBadge();
      this.toggleHistoryPanel();
      const expanded = this.historyPanelEl !== null;
      btn.setAttribute("aria-expanded", String(expanded));
    });

    return btn;
  }

  renderHistoryPanel(): HTMLElement {
    const panel = document.createElement("div");
    panel.className = "notification-history";
    panel.setAttribute("role", "log");

    const header = document.createElement("div");
    header.className = "notification-history-header";

    const title = document.createElement("span");
    title.className = "notification-history-title";
    title.textContent = "Notifications";

    const clearBtn = document.createElement("button");
    clearBtn.className = "notification-history-clear";
    clearBtn.textContent = "Clear All";
    clearBtn.addEventListener("click", () => {
      this.clearHistory();
      this.renderHistoryList(panel);
    });

    header.appendChild(title);
    header.appendChild(clearBtn);
    panel.appendChild(header);

    this.renderHistoryList(panel);
    return panel;
  }

  toggleHistoryPanel(): void {
    if (this.historyPanelEl) {
      this.historyPanelEl.remove();
      this.historyPanelEl = null;
      return;
    }

    this.historyPanelEl = this.renderHistoryPanel();
    document.body.appendChild(this.historyPanelEl);
  }

  private renderHistoryList(panel: HTMLElement): void {
    const existing = panel.querySelector(".notification-history-list");
    if (existing) existing.remove();

    const list = document.createElement("div");
    list.className = "notification-history-list";

    if (this.history.length === 0) {
      const empty = document.createElement("div");
      empty.className = "notification-history-empty";
      empty.textContent = "No notifications";
      list.appendChild(empty);
    } else {
      for (const item of this.history) {
        list.appendChild(this.createHistoryItem(item));
      }
    }

    panel.appendChild(list);
  }

  private createHistoryItem(item: NotificationItem): HTMLElement {
    const row = document.createElement("div");
    row.className = `notification-history-item notification-history-item-${item.type}`;

    const icon = document.createElement("span");
    icon.className = "notification-history-icon";
    icon.textContent = TYPE_ICONS[item.type];

    const body = document.createElement("div");
    body.className = "notification-history-body";

    const msg = document.createElement("div");
    msg.className = "notification-history-message";
    msg.textContent = item.message;

    const time = document.createElement("span");
    time.className = "notification-history-time";
    time.textContent = this.relativeTime(item.timestamp);

    body.appendChild(msg);

    if (item.actions.length > 0) {
      const actions = document.createElement("div");
      actions.className = "notification-history-actions";
      for (const action of item.actions) {
        const btn = document.createElement("button");
        btn.className = "notification-history-action-btn";
        btn.textContent = action.label;
        btn.addEventListener("click", action.callback);
        actions.appendChild(btn);
      }
      body.appendChild(actions);
    }

    body.appendChild(time);
    row.appendChild(icon);
    row.appendChild(body);
    return row;
  }

  private ensureContainer(): HTMLElement {
    if (this.container) return this.container;

    this.container = document.createElement("div");
    this.container.id = "notification-container";
    this.container.setAttribute("role", "alert");
    this.container.setAttribute("aria-live", "assertive");
    document.body.appendChild(this.container);
    return this.container;
  }

  private presentToast(
    item: NotificationItem,
    options: NotificationOptions,
  ): void {
    const container = this.ensureContainer();

    const toast = document.createElement("div");
    toast.className = `toast toast-${item.type}`;
    toast.setAttribute("data-notification-id", item.id);
    toast.dataset.testid =
      item.type === "error" ? "notification-error" : "notification";

    const iconEl = document.createElement("div");
    iconEl.className = "toast-icon";
    iconEl.textContent = TYPE_ICONS[item.type];

    const body = document.createElement("div");
    body.className = "toast-body";

    const msgEl = document.createElement("div");
    msgEl.className = "toast-message";
    msgEl.textContent = item.message;
    body.appendChild(msgEl);

    if (item.actions.length > 0) {
      const actionsEl = document.createElement("div");
      actionsEl.className = "toast-actions";
      for (const action of item.actions) {
        actionsEl.appendChild(this.createActionButton(action, item));
      }
      body.appendChild(actionsEl);
    }

    const closeBtn = document.createElement("button");
    closeBtn.className = "toast-close";
    closeBtn.textContent = "×";
    closeBtn.setAttribute("aria-label", "Dismiss notification");
    closeBtn.addEventListener("click", () => this.removeToast(item));

    toast.appendChild(iconEl);
    toast.appendChild(body);
    toast.appendChild(closeBtn);

    container.appendChild(toast);
    item.element = toast;
    this.visible.push(item);

    // Double rAF ensures the browser registers the initial class before removing
    // it — a single frame is insufficient to trigger the CSS transition
    toast.classList.add("toast-enter");
    requestAnimationFrame(() => {
      requestAnimationFrame(() => {
        toast.classList.remove("toast-enter");
      });
    });

    if (!options.persistent) {
      const timeout =
        options.timeout ??
        (item.type === "error" ? ERROR_TIMEOUT : DEFAULT_TIMEOUT);
      item.dismissTimer = window.setTimeout(
        () => this.removeToast(item),
        timeout,
      );
    }
  }

  private createActionButton(
    action: NotificationAction,
    item: NotificationItem,
  ): HTMLButtonElement {
    const btn = document.createElement("button");
    btn.className = "toast-action-btn";
    btn.textContent = action.label;
    btn.addEventListener("click", () => {
      action.callback();
      this.removeToast(item);
    });
    return btn;
  }

  private removeToast(item: NotificationItem): void {
    if (item.dismissTimer !== null) {
      clearTimeout(item.dismissTimer);
      item.dismissTimer = null;
    }

    if (!item.element) return;

    item.element.classList.add("toast-exit");
    const el = item.element;

    setTimeout(() => {
      el.remove();
    }, 300);

    item.element = null;
    const idx = this.visible.indexOf(item);
    if (idx !== -1) {
      this.visible.splice(idx, 1);
    }

    this.showNextQueued();
  }

  private showNextQueued(): void {
    if (this.queue.length === 0) return;
    if (this.visible.length >= MAX_VISIBLE) return;

    const next = this.queue.shift()!;
    this.presentToast(next, {
      message: next.message,
      type: next.type,
      actions: next.actions,
      persistent: next.persistent,
    });
  }

  private addToHistory(item: NotificationItem): void {
    this.history.unshift(item);
    if (this.history.length > MAX_HISTORY) {
      this.history.pop();
    }
  }

  private relativeTime(date: Date): string {
    const diffMs = Date.now() - date.getTime();
    const diffSec = Math.floor(diffMs / 1000);
    const diffMin = Math.floor(diffSec / 60);
    const diffHr = Math.floor(diffMin / 60);

    if (diffSec < 60) return "just now";
    if (diffMin < 60) return `${diffMin}m ago`;
    if (diffHr < 24) return `${diffHr}h ago`;
    return `${Math.floor(diffHr / 24)}d ago`;
  }

  private systemNotify(title: string, body: string): void {
    if (!this.enabled) return;
    if (document.hasFocus()) return;
    if (!("Notification" in window)) return;
    if (Notification.permission !== "granted") return;

    const notification = new Notification(title, { body });
    notification.onclick = () => {
      window.focus();
      notification.close();
    };
  }

  private playErrorSound(): void {
    if (typeof AudioContext === "undefined") return;

    const ctx = new AudioContext();
    const osc = ctx.createOscillator();
    const gain = ctx.createGain();

    osc.type = "sine";
    osc.frequency.value = 440;
    gain.gain.value = 0.1;

    osc.connect(gain);
    gain.connect(ctx.destination);

    osc.start();
    osc.stop(ctx.currentTime + 0.2);

    osc.onended = () => ctx.close();
  }
}
