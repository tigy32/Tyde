export const isMac: boolean =
  navigator.platform.toUpperCase().indexOf("MAC") >= 0;

export function formatShortcut(key: string): string {
  if (!isMac) return key;

  return key
    .replace(/Ctrl\+/g, "⌘")
    .replace(/Shift\+/g, "⇧")
    .replace(/Alt\+/g, "⌥");
}

// Layered escape handling — priority is determined by push order (LIFO).
// Consumers push handlers when their UI layer activates (e.g. command palette,
// lightbox, context menu) and remove them on close, so Escape always dismisses
// the topmost layer first.
export class EscapeStack {
  private stack: { id: string; handler: () => void }[] = [];

  push(id: string, handler: () => void): void {
    this.stack.push({ id, handler });
  }

  remove(id: string): void {
    this.stack = this.stack.filter((entry) => entry.id !== id);
  }

  handle(): boolean {
    if (this.stack.length === 0) return false;
    const top = this.stack.pop()!;
    top.handler();
    return true;
  }
}

interface ParsedShortcut {
  ctrl: boolean;
  shift: boolean;
  alt: boolean;
  key: string;
}

function parseShortcut(shortcut: string): ParsedShortcut {
  let ctrl = false;
  let shift = false;
  let alt = false;
  let remaining = shortcut;

  if (remaining.startsWith("Ctrl+")) {
    ctrl = true;
    remaining = remaining.slice(5);
  }
  if (remaining.startsWith("Shift+")) {
    shift = true;
    remaining = remaining.slice(6);
  }
  if (remaining.startsWith("Alt+")) {
    alt = true;
    remaining = remaining.slice(4);
  }

  return { ctrl, shift, alt, key: remaining.toLowerCase() };
}

export class KeyboardManager {
  private escapeStack: EscapeStack;
  private shortcuts: { parsed: ParsedShortcut; handler: () => void }[] = [];
  private escapeHandler: (() => void) | null = null;
  private enabled = false;

  constructor(escapeStack: EscapeStack) {
    this.escapeStack = escapeStack;
  }

  register(shortcut: string, handler: () => void): void {
    if (shortcut === "Escape") {
      this.escapeHandler = handler;
      return;
    }
    this.shortcuts.push({ parsed: parseShortcut(shortcut), handler });
  }

  enable(): void {
    if (this.enabled) return;
    this.enabled = true;
    document.addEventListener("keydown", (e: KeyboardEvent) => {
      const bypass = this.shouldBypassShortcuts(e.target);

      if (e.key === "Escape") {
        if (bypass) return;
        if (this.escapeStack.handle()) {
          e.preventDefault();
          return;
        }
        if (this.escapeHandler) {
          e.preventDefault();
          this.escapeHandler();
        }
        return;
      }

      if (bypass) return;

      const hasModifier = e.ctrlKey || e.metaKey;

      for (const entry of this.shortcuts) {
        const p = entry.parsed;
        if (p.ctrl && !hasModifier) continue;
        if (!p.ctrl && hasModifier) continue;
        if (p.shift !== e.shiftKey) continue;
        if (p.alt !== e.altKey) continue;
        if (e.key.toLowerCase() !== p.key) continue;

        e.preventDefault();
        entry.handler();
        return;
      }
    });
  }

  private shouldBypassShortcuts(target: EventTarget | null): boolean {
    if (!(target instanceof HTMLElement)) return false;
    return target.closest('[data-keyboard-shortcuts="off"]') !== null;
  }
}

let cheatSheetOverlay: HTMLElement | null = null;

// Standalone cheat sheet functions need the escape stack but cannot accept
// parameters (API contract), so we hold the reference at module scope.
let activeEscapeStack: EscapeStack | null = null;

export function setCheatSheetEscapeStack(stack: EscapeStack): void {
  activeEscapeStack = stack;
}

export function showCheatSheet(): void {
  if (cheatSheetOverlay) return;

  const groups = [
    {
      title: "General",
      entries: [
        { label: "Command Palette", key: "Ctrl+K" },
        { label: "Settings", key: "Ctrl+," },
        { label: "New Conversation", key: "Ctrl+N" },
        { label: "Cheat Sheet", key: "Ctrl+/" },
        { label: "Find in File", key: "Ctrl+F" },
        { label: "Go to Line", key: "Ctrl+G" },
      ],
    },
    {
      title: "Chat",
      entries: [
        { label: "Send Message", key: "Enter" },
        { label: "New Line", key: "Shift+Enter" },
        { label: "Cancel Request", key: "Escape" },
        { label: "Clear Chat", key: "Ctrl+L" },
      ],
    },
    {
      title: "Navigation",
      entries: [
        { label: "Focus Chat", key: "Ctrl+1" },
        { label: "Git Panel", key: "Ctrl+2" },
        { label: "File Explorer", key: "Ctrl+3" },
        { label: "Diff Panel", key: "Ctrl+4" },
        { label: "Settings", key: "Ctrl+5" },
      ],
    },
    {
      title: "Layout",
      entries: [
        { label: "Toggle Right Panel", key: "Ctrl+B" },
        { label: "Toggle Full-Screen", key: "Ctrl+Shift+F" },
        { label: "Toggle Task List", key: "Ctrl+J" },
        { label: "Increase Font Size", key: "Ctrl+=" },
        { label: "Decrease Font Size", key: "Ctrl+-" },
      ],
    },
  ];

  const overlay = document.createElement("div");
  overlay.className = "shortcut-cheatsheet-overlay";

  const container = document.createElement("div");
  container.className = "shortcut-cheatsheet";

  const title = document.createElement("h2");
  title.textContent = "Keyboard Shortcuts";
  title.style.margin = "0 0 16px";
  container.appendChild(title);

  for (const group of groups) {
    const groupEl = document.createElement("div");
    groupEl.className = "shortcut-group";

    const groupTitle = document.createElement("div");
    groupTitle.className = "shortcut-group-title";
    groupTitle.textContent = group.title;
    groupEl.appendChild(groupTitle);

    for (const entry of group.entries) {
      const row = document.createElement("div");
      row.className = "shortcut-entry";

      const labelSpan = document.createElement("span");
      labelSpan.className = "shortcut-entry-label";
      labelSpan.textContent = entry.label;

      const keySpan = document.createElement("kbd");
      keySpan.className = "shortcut-entry-key";
      keySpan.textContent = formatShortcut(entry.key);

      row.appendChild(labelSpan);
      row.appendChild(keySpan);
      groupEl.appendChild(row);
    }

    container.appendChild(groupEl);
  }

  overlay.appendChild(container);

  overlay.addEventListener("click", (e) => {
    if (e.target === overlay) hideCheatSheet();
  });

  document.body.appendChild(overlay);
  cheatSheetOverlay = overlay;

  activeEscapeStack?.push("cheatsheet", hideCheatSheet);
}

export function hideCheatSheet(): void {
  if (!cheatSheetOverlay) return;
  cheatSheetOverlay.remove();
  cheatSheetOverlay = null;
  activeEscapeStack?.remove("cheatsheet");
}

export function isCheatSheetVisible(): boolean {
  return cheatSheetOverlay !== null;
}
