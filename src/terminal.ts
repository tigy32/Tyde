import type { UnlistenFn } from "@tauri-apps/api/event";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal as XtermTerminal } from "@xterm/xterm";
import {
  closeTerminal,
  createTerminal,
  onTerminalExit,
  onTerminalOutput,
  resizeTerminal,
  writeTerminal,
} from "./bridge";
import "@xterm/xterm/css/xterm.css";

interface TerminalSession {
  id: number;
  label: string;
  viewEl: HTMLElement;
  xterm: XtermTerminal;
  fit: FitAddon;
  exited: boolean;
  resizeObserver: ResizeObserver;
}

export interface CreatedTerminalSession {
  id: number;
  label: string;
  viewEl: HTMLElement;
}

export class TerminalService {
  private readonly workspacePath: string;

  private readonly sessions = new Map<number, TerminalSession>();
  private labelCounter = 1;
  private destroyed = false;
  private fontSizePx = 13;
  private rootStyleObserver: MutationObserver | null = null;

  private unlistenOutput: UnlistenFn | null = null;
  private unlistenExit: UnlistenFn | null = null;

  onTitleChange: ((terminalId: number, title: string) => void) | null = null;
  onExit: ((terminalId: number) => void) | null = null;

  constructor(workspacePath: string) {
    this.workspacePath = workspacePath;
    this.fontSizePx = this.resolveFontSize();
    this.rootStyleObserver = new MutationObserver(() => {
      this.handleBaseFontSizeMaybeChanged();
    });
    this.rootStyleObserver.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["style"],
    });
    void this.bindTerminalEvents();
  }

  async createSession(): Promise<CreatedTerminalSession> {
    if (this.destroyed) {
      throw new Error("Terminal service is disposed");
    }

    const id = await createTerminal(this.workspacePath);
    if (!Number.isFinite(id)) {
      throw new Error("Backend returned an invalid terminal id");
    }

    const session = this.buildSession(id);
    this.sessions.set(id, session);
    this.focus(id);

    return {
      id,
      label: session.label,
      viewEl: session.viewEl,
    };
  }

  async closeSession(terminalId: number): Promise<void> {
    const session = this.sessions.get(terminalId);
    if (!session) return;

    this.sessions.delete(terminalId);
    session.resizeObserver.disconnect();
    session.xterm.dispose();
    session.viewEl.remove();

    try {
      await closeTerminal(terminalId);
    } catch (err) {
      console.error("Failed to close terminal:", err);
      return;
    }
  }

  focus(terminalId: number): void {
    const session = this.sessions.get(terminalId);
    if (!session) return;
    this.fitAndResize(session);
    session.xterm.focus();
  }

  destroy(): void {
    if (this.destroyed) return;
    this.destroyed = true;

    this.rootStyleObserver?.disconnect();
    this.rootStyleObserver = null;

    if (this.unlistenOutput) {
      this.unlistenOutput();
      this.unlistenOutput = null;
    }
    if (this.unlistenExit) {
      this.unlistenExit();
      this.unlistenExit = null;
    }

    for (const session of this.sessions.values()) {
      session.resizeObserver.disconnect();
      session.xterm.dispose();
      session.viewEl.remove();
      void closeTerminal(session.id).catch((err) =>
        console.error("Failed to close terminal on destroy:", err),
      );
    }
    this.sessions.clear();
  }

  private async bindTerminalEvents(): Promise<void> {
    const unlistenOutput = await onTerminalOutput((payload) => {
      if (this.destroyed) return;
      const session = this.sessions.get(payload.terminal_id);
      if (!session) return;
      session.xterm.write(payload.data);
    });

    if (this.destroyed) {
      unlistenOutput();
      return;
    }
    this.unlistenOutput = unlistenOutput;

    const unlistenExit = await onTerminalExit((payload) => {
      if (this.destroyed) return;
      const session = this.sessions.get(payload.terminal_id);
      if (!session || session.exited) return;
      session.exited = true;
      session.xterm.write("\r\n\x1b[90m[process exited]\x1b[0m\r\n");
      this.onExit?.(payload.terminal_id);
    });

    if (this.destroyed) {
      unlistenExit();
      return;
    }
    this.unlistenExit = unlistenExit;
  }

  private buildSession(id: number): TerminalSession {
    const viewEl = document.createElement("div");
    viewEl.className = "terminal-view";
    viewEl.dataset.terminalId = String(id);
    viewEl.dataset.keyboardShortcuts = "off";

    const hostEl = document.createElement("div");
    hostEl.className = "terminal-view-host";
    viewEl.appendChild(hostEl);

    const xterm = new XtermTerminal({
      cursorBlink: true,
      scrollback: 8000,
      fontFamily:
        'Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace',
      fontSize: this.fontSizePx,
      macOptionIsMeta: true,
      allowTransparency: true,
    });
    const fit = new FitAddon();
    xterm.loadAddon(fit);
    xterm.open(hostEl);

    const label = `Terminal ${this.labelCounter++}`;

    xterm.onData((data) => {
      void writeTerminal(id, data).catch((err) =>
        console.error("Failed to write to terminal:", err),
      );
    });

    xterm.onResize(({ cols, rows }) => {
      if (cols <= 0 || rows <= 0) return;
      void resizeTerminal(id, cols, rows).catch((err) =>
        console.error("Failed to resize terminal:", err),
      );
    });

    xterm.onTitleChange((title) => {
      const trimmed = title.trim();
      if (!trimmed) return;
      const current = this.sessions.get(id);
      if (!current) return;
      current.label = trimmed;
      this.onTitleChange?.(id, trimmed);
    });

    const resizeObserver = new ResizeObserver(() => {
      const current = this.sessions.get(id);
      if (!current || this.destroyed) return;
      this.fitAndResize(current);
    });
    resizeObserver.observe(viewEl);

    return {
      id,
      label,
      viewEl,
      xterm,
      fit,
      exited: false,
      resizeObserver,
    };
  }

  private fitAndResize(session: TerminalSession): void {
    if (!this.isVisible(session.viewEl)) return;
    try {
      session.fit.fit();
    } catch (err) {
      console.warn("Terminal resize failed:", err);
      return;
    }

    const cols = session.xterm.cols;
    const rows = session.xterm.rows;
    if (cols <= 0 || rows <= 0) return;
    void resizeTerminal(session.id, cols, rows).catch((err) =>
      console.error("Failed to resize terminal after fit:", err),
    );
  }

  private isVisible(el: HTMLElement): boolean {
    if (!el.isConnected) return false;
    const rect = el.getBoundingClientRect();
    return rect.width > 40 && rect.height > 20 && el.offsetParent !== null;
  }

  private resolveFontSize(): number {
    const raw = getComputedStyle(document.documentElement).getPropertyValue(
      "--base-font-size",
    );
    const parsed = Number.parseFloat(raw);
    if (!Number.isFinite(parsed)) return 13;
    return Math.min(28, Math.max(8, parsed));
  }

  private handleBaseFontSizeMaybeChanged(): void {
    const nextSize = this.resolveFontSize();
    if (nextSize === this.fontSizePx) return;
    this.fontSizePx = nextSize;

    for (const session of this.sessions.values()) {
      session.xterm.options.fontSize = nextSize;
      this.fitAndResize(session);
    }
  }
}
