import { logTabPerf, perfNow } from "./perf_debug";
import type { PanelFactory, PanelType, TilingNode } from "./tiling/types";

type DockZone = "left" | "right" | "bottom";
export type PersistentWidgetId =
  | "git"
  | "files"
  | "sessions"
  | "agents"
  | "terminal"
  | "workflows";
type WidgetId = PersistentWidgetId;
type DropTarget = DockZone;
type CenterView = "chat" | "editor" | "home";

interface WorkbenchState {
  widgetZones: Record<PersistentWidgetId, DockZone>;
  widgetOrder: Record<DockZone, WidgetId[]>;
  activeWidget: Record<DockZone, WidgetId | null>;
  centerView: CenterView;
  leftVisible: boolean;
  rightVisible: boolean;
  bottomVisible: boolean;
  fullScreenChat: boolean;
  leftWidth: number;
  rightWidth: number;
  bottomHeight: number;
}

interface DockZoneElements {
  el: HTMLElement;
  tabsEl: HTMLElement;
  contentEl: HTMLElement;
}

const STATE_MARKER = "__WORKBENCH_LAYOUT_V1__";

const WIDGET_TITLES: Record<WidgetId, string> = {
  git: "Git",
  files: "Files",
  sessions: "History",
  agents: "Agents",
  terminal: "Terminal",
  workflows: "Workflows",
};

const WIDGET_TO_PANEL: Record<WidgetId, PanelType> = {
  git: "git",
  files: "explorer",
  sessions: "sessions",
  agents: "agents",
  terminal: "terminal",
  workflows: "workflows",
};

const ALL_WIDGETS: PersistentWidgetId[] = [
  "git",
  "files",
  "sessions",
  "agents",
  "terminal",
  "workflows",
];

function defaultState(): WorkbenchState {
  return {
    widgetZones: {
      files: "left",
      git: "left",
      sessions: "right",
      agents: "right",
      terminal: "bottom",
      workflows: "right",
    },
    widgetOrder: {
      left: ["files", "git"],
      right: ["agents", "sessions", "workflows"],
      bottom: ["terminal"],
    },
    activeWidget: {
      left: "files",
      right: "agents",
      bottom: "terminal",
    },
    centerView: "chat",
    leftVisible: true,
    rightVisible: true,
    bottomVisible: false,
    fullScreenChat: false,
    leftWidth: 320,
    rightWidth: 320,
    bottomHeight: 250,
  };
}

function isDockZone(value: unknown): value is DockZone {
  return value === "left" || value === "right" || value === "bottom";
}

function isWidgetId(value: unknown): value is WidgetId {
  return (
    value === "git" ||
    value === "files" ||
    value === "sessions" ||
    value === "agents" ||
    value === "terminal" ||
    value === "workflows"
  );
}

function isCenterView(value: unknown): value is CenterView {
  return value === "chat" || value === "editor" || value === "home";
}

function sanitizeState(raw: unknown): WorkbenchState {
  const fallback = defaultState();
  if (!raw || typeof raw !== "object") return fallback;

  const src = raw as Partial<WorkbenchState>;
  const widgetZones: Record<PersistentWidgetId, DockZone> = {
    ...fallback.widgetZones,
  };
  if (src.widgetZones && typeof src.widgetZones === "object") {
    for (const widget of ALL_WIDGETS) {
      const zone = (src.widgetZones as Record<string, unknown>)[widget];
      if (isDockZone(zone)) widgetZones[widget] = zone;
    }
  }

  const widgetOrder: Record<DockZone, WidgetId[]> = {
    left: [],
    right: [],
    bottom: [],
  };

  if (src.widgetOrder && typeof src.widgetOrder === "object") {
    for (const zone of ["left", "right", "bottom"] as const) {
      const list = (src.widgetOrder as Record<string, unknown>)[zone];
      if (!Array.isArray(list)) continue;
      for (const item of list) {
        if (!isWidgetId(item)) continue;
        if (widgetOrder[zone].includes(item)) continue;
        widgetOrder[zone].push(item);
      }
    }
  }

  // Ensure every widget is present exactly once, respecting preferred zone.
  for (const widget of ALL_WIDGETS) {
    const zone = widgetZones[widget];
    const alreadyPlaced =
      widgetOrder.left.includes(widget) ||
      widgetOrder.right.includes(widget) ||
      widgetOrder.bottom.includes(widget);
    if (!alreadyPlaced) {
      widgetOrder[zone].push(widget);
      continue;
    }
    // If placed but not in its resolved zone, move it.
    for (const z of ["left", "right", "bottom"] as const) {
      if (z === zone) continue;
      const idx = widgetOrder[z].indexOf(widget);
      if (idx !== -1) {
        widgetOrder[z].splice(idx, 1);
        if (!widgetOrder[zone].includes(widget)) widgetOrder[zone].push(widget);
      }
    }
  }

  const activeWidget: Record<DockZone, WidgetId | null> = {
    left: fallback.activeWidget.left,
    right: fallback.activeWidget.right,
    bottom: fallback.activeWidget.bottom,
  };

  if (src.activeWidget && typeof src.activeWidget === "object") {
    for (const zone of ["left", "right", "bottom"] as const) {
      const candidate = (src.activeWidget as Record<string, unknown>)[zone];
      if (isWidgetId(candidate) && widgetOrder[zone].includes(candidate)) {
        activeWidget[zone] = candidate;
      }
    }
  }

  for (const zone of ["left", "right", "bottom"] as const) {
    if (
      !activeWidget[zone] ||
      !widgetOrder[zone].includes(activeWidget[zone] as WidgetId)
    ) {
      activeWidget[zone] = widgetOrder[zone][0] ?? null;
    }
  }

  const migrated = {
    widgetZones,
    widgetOrder,
    activeWidget,
    centerView: isCenterView(src.centerView)
      ? src.centerView
      : fallback.centerView,
    leftVisible:
      typeof src.leftVisible === "boolean"
        ? src.leftVisible
        : fallback.leftVisible,
    rightVisible:
      typeof src.rightVisible === "boolean"
        ? src.rightVisible
        : fallback.rightVisible,
    bottomVisible:
      typeof src.bottomVisible === "boolean"
        ? src.bottomVisible
        : fallback.bottomVisible,
    fullScreenChat:
      typeof src.fullScreenChat === "boolean"
        ? src.fullScreenChat
        : fallback.fullScreenChat,
    leftWidth:
      typeof src.leftWidth === "number" ? src.leftWidth : fallback.leftWidth,
    rightWidth:
      typeof src.rightWidth === "number" ? src.rightWidth : fallback.rightWidth,
    bottomHeight:
      typeof src.bottomHeight === "number"
        ? src.bottomHeight
        : fallback.bottomHeight,
  };

  return migrateLegacyDefaultDocking(migrated);
}

function sameMembers(list: WidgetId[], expected: WidgetId[]): boolean {
  if (list.length !== expected.length) return false;
  return expected.every((item) => list.includes(item));
}

// One-time migration from previous default:
// left=[files,git], right=[], bottom=[sessions]
// -> left=[files,git], right=[sessions], bottom=[]
function migrateLegacyDefaultDocking(state: WorkbenchState): WorkbenchState {
  const leftLegacy = sameMembers(state.widgetOrder.left, ["files", "git"]);
  const rightLegacy = state.widgetOrder.right.length === 0;
  const bottomLegacy =
    sameMembers(state.widgetOrder.bottom, ["sessions"]) ||
    sameMembers(state.widgetOrder.bottom, ["sessions", "terminal"]);
  const zonesLegacy =
    state.widgetZones.files === "left" &&
    state.widgetZones.git === "left" &&
    state.widgetZones.sessions === "bottom" &&
    state.widgetZones.terminal === "bottom";

  if (!leftLegacy || !rightLegacy || !bottomLegacy || !zonesLegacy) {
    return state;
  }

  return {
    ...state,
    widgetZones: {
      ...state.widgetZones,
      sessions: "right",
    },
    widgetOrder: {
      left: [...state.widgetOrder.left],
      right: ["sessions"],
      bottom: ["terminal"],
    },
    activeWidget: {
      left: state.activeWidget.left ?? "files",
      right: "agents",
      bottom: "terminal",
    },
    rightVisible: true,
    bottomVisible: false,
  };
}

function serializedToTree(state: WorkbenchState): TilingNode {
  const payload = JSON.stringify({ marker: STATE_MARKER, state });
  return {
    kind: "leaf",
    id: "workbench-state",
    tabs: [{ id: "workbench-state-tab", type: "chat", title: payload }],
    activeTabIndex: 0,
  } as TilingNode;
}

function treeToSerializedState(tree: TilingNode): WorkbenchState | null {
  const payload = (tree as any)?.tabs?.[0]?.title;
  if (typeof payload !== "string") return null;
  try {
    const parsed = JSON.parse(payload);
    if (parsed?.marker !== STATE_MARKER) return null;
    return sanitizeState(parsed.state);
  } catch {
    return null;
  }
}

export class Layout {
  private container: HTMLElement;
  private workspacePath: string;
  private storageKey: string;

  private state: WorkbenchState;

  private rootEl!: HTMLElement;
  private topEl!: HTMLElement;
  private centerEl!: HTMLElement;
  private centerTabsEl!: HTMLElement;
  private centerTabsTrackEl!: HTMLElement;
  private centerTabsActionsEl!: HTMLElement;
  private centerContentEl!: HTMLElement;
  private chatViewEl!: HTMLElement;
  private editorViewEl!: HTMLElement;
  private homeViewEl!: HTMLElement;
  private homeMode = false;
  private chatShellEl: HTMLElement | null = null;

  private dockedConversations = new Map<
    number,
    { zone: DockZone; wrapperEl: HTMLElement; title: string }
  >();
  private dockedTerminals = new Map<
    number,
    { zone: DockZone; wrapperEl: HTMLElement; title: string; exited: boolean }
  >();
  private activeDockConversation: Record<DockZone, number | null> = {
    left: null,
    right: null,
    bottom: null,
  };
  private activeDockTerminal: Record<DockZone, number | null> = {
    left: null,
    right: null,
    bottom: null,
  };
  private tabDragDropZone: DockZone | null = null;

  onUndockConversation: ((conversationId: number) => void) | null = null;
  onCreateTerminal: ((zone: DockZone) => void) | null = null;
  onCloseTerminal: ((terminalId: number) => void) | null = null;
  onActivateTerminal: ((terminalId: number) => void) | null = null;

  private leftZone!: DockZoneElements;
  private rightZone!: DockZoneElements;
  private bottomZone!: DockZoneElements;

  private leftHandle!: HTMLElement;
  private rightHandle!: HTMLElement;
  private bottomHandle!: HTMLElement;

  private resizeTarget: "left" | "right" | "bottom" | null = null;
  private resizeStartPos = 0;
  private resizeStartSize = 0;
  private resizeRafId = 0;

  private widgetPanels = new Map<WidgetId, HTMLElement>();

  private draggingWidget: WidgetId | null = null;
  private draggingPointerId: number | null = null;
  private dragStartX = 0;
  private dragStartY = 0;
  private dragActive = false;
  private dropZone: DropTarget | null = null;
  private widgetDropOverlays: HTMLElement[] = [];
  private suppressClickWidget: WidgetId | null = null;
  private pointerMoveHandler: ((e: PointerEvent) => void) | null = null;
  private pointerUpHandler: ((e: PointerEvent) => void) | null = null;

  private draggingConversation: number | null = null;
  private draggingConvPointerId: number | null = null;
  private convDragStartX = 0;
  private convDragStartY = 0;
  private convDragActive = false;
  private convDropOnCenter = false;
  private suppressClickConversation: number | null = null;
  private convPointerMoveHandler: ((e: PointerEvent) => void) | null = null;
  private convPointerUpHandler: ((e: PointerEvent) => void) | null = null;

  private draggingTerminal: number | null = null;
  private draggingTerminalPointerId: number | null = null;
  private terminalDragStartX = 0;
  private terminalDragStartY = 0;
  private terminalDragActive = false;
  private suppressClickTerminal: number | null = null;
  private terminalPointerMoveHandler: ((e: PointerEvent) => void) | null = null;
  private terminalPointerUpHandler: ((e: PointerEvent) => void) | null = null;
  private availableWidgets: Set<PersistentWidgetId>;

  constructor(
    container: HTMLElement,
    panelFactory: PanelFactory,
    workspacePath: string,
    options?: { availableWidgets?: PersistentWidgetId[] },
  ) {
    this.container = container;
    this.workspacePath = workspacePath;
    this.storageKey = `workbench-layout:${this.workspacePath}`;
    this.availableWidgets = new Set(options?.availableWidgets ?? ALL_WIDGETS);

    this.widgetPanels.set("git", panelFactory(WIDGET_TO_PANEL.git));
    this.widgetPanels.set("files", panelFactory(WIDGET_TO_PANEL.files));
    this.widgetPanels.set("sessions", panelFactory(WIDGET_TO_PANEL.sessions));
    this.widgetPanels.set("agents", panelFactory(WIDGET_TO_PANEL.agents));
    this.widgetPanels.set("terminal", panelFactory(WIDGET_TO_PANEL.terminal));
    this.widgetPanels.set("workflows", panelFactory(WIDGET_TO_PANEL.workflows));

    const persisted = this.loadPersistedState();
    this.state = persisted ?? defaultState();

    this.buildDom(panelFactory);
    this.render();
  }

  switchTab(tabId: string): void {
    const start = perfNow();
    let renderMs = 0;

    if (tabId === "home") {
      if (this.state.centerView === "home") {
        logTabPerf("Layout.switchTab", perfNow() - start, {
          tabId,
          centerView: this.state.centerView,
          skipped: true,
        });
        return;
      }
      this.state.centerView = "home";
      const renderStart = perfNow();
      this.render();
      renderMs = perfNow() - renderStart;
      logTabPerf("Layout.switchTab", perfNow() - start, {
        tabId,
        centerView: this.state.centerView,
        renderMs,
      });
      return;
    }

    if (this.homeMode) return;

    if (tabId === "chat") {
      if (this.state.centerView === "chat") {
        logTabPerf("Layout.switchTab", perfNow() - start, {
          tabId,
          centerView: this.state.centerView,
          skipped: true,
        });
        return;
      }
      this.state.centerView = "chat";
      const renderStart = perfNow();
      this.render();
      renderMs = perfNow() - renderStart;
      logTabPerf("Layout.switchTab", perfNow() - start, {
        tabId,
        centerView: this.state.centerView,
        renderMs,
      });
      return;
    }

    if (tabId === "diff") {
      if (this.state.centerView === "editor") {
        logTabPerf("Layout.switchTab", perfNow() - start, {
          tabId,
          centerView: this.state.centerView,
          skipped: true,
        });
        return;
      }
      this.state.centerView = "editor";
      const renderStart = perfNow();
      this.render();
      renderMs = perfNow() - renderStart;
      logTabPerf("Layout.switchTab", perfNow() - start, {
        tabId,
        centerView: this.state.centerView,
        renderMs,
      });
      return;
    }

    if (tabId === "git") {
      this.showWidget("git");
      logTabPerf("Layout.switchTab", perfNow() - start, {
        tabId,
        centerView: this.state.centerView,
        via: "showWidget",
      });
      return;
    }

    if (tabId === "files") {
      this.showWidget("files");
      logTabPerf("Layout.switchTab", perfNow() - start, {
        tabId,
        centerView: this.state.centerView,
        via: "showWidget",
      });
      return;
    }

    if (tabId === "terminal") {
      this.showWidget("terminal");
      logTabPerf("Layout.switchTab", perfNow() - start, {
        tabId,
        centerView: this.state.centerView,
        via: "showWidget",
      });
    }
  }

  setCenterTabBars(
    chatTabBarEl: HTMLElement,
    editorTabBarEl: HTMLElement | null = null,
  ): void {
    chatTabBarEl.classList.add("center-chat-tab-bar");
    this.centerTabsTrackEl.innerHTML = "";
    this.centerTabsTrackEl.appendChild(chatTabBarEl);
    if (editorTabBarEl) {
      editorTabBarEl.classList.add("editor-tab-strip");
      this.centerTabsTrackEl.appendChild(editorTabBarEl);
    }
  }

  setCenterTabActions(actionsEl: HTMLElement | null): void {
    this.centerTabsActionsEl.innerHTML = "";
    if (!actionsEl) return;
    this.centerTabsActionsEl.appendChild(actionsEl);
  }

  registerChatTabBar(_tabBarEl: HTMLElement): void {
    // Placeholder for future use
  }

  showWidget(widget: PersistentWidgetId): void {
    if (!this.isWidgetAvailable(widget)) return;
    const zone = this.state.widgetZones[widget];
    this.activeDockConversation[zone] = null;
    this.activeDockTerminal[zone] = null;
    this.state.activeWidget[zone] = widget;
    if (zone === "left") this.state.leftVisible = true;
    if (zone === "right") this.state.rightVisible = true;
    if (zone === "bottom") this.state.bottomVisible = true;
    this.render();
  }

  toggleLeftPanel(): void {
    if (this.state.fullScreenChat) return;
    this.state.leftVisible = !this.state.leftVisible;
    this.render();
  }

  toggleRightPanel(): void {
    if (this.state.fullScreenChat) return;
    this.state.rightVisible = !this.state.rightVisible;
    this.render();
  }

  toggleBottomPanel(): void {
    if (this.state.fullScreenChat) return;
    this.state.bottomVisible = !this.state.bottomVisible;
    this.render();
  }

  ensureRightPanelVisible(): void {
    this.state.rightVisible = true;
    this.render();
  }

  setHomeMode(enabled: boolean): void {
    this.homeMode = enabled;
    if (enabled) {
      this.state.centerView = "home";
    }
    this.render();
  }

  getHomeViewEl(): HTMLElement {
    return this.homeViewEl;
  }

  toggleFullScreenChat(): void {
    this.state.fullScreenChat = !this.state.fullScreenChat;
    if (this.state.fullScreenChat) {
      this.state.centerView = "chat";
    }
    this.render();
  }

  isFullScreenChat(): boolean {
    return this.state.fullScreenChat;
  }

  getActiveTab(): string {
    return this.state.centerView === "editor" ? "diff" : "chat";
  }

  getLayoutTree(): TilingNode {
    return serializedToTree(this.state);
  }

  setLayoutTree(tree: TilingNode): void {
    const decoded = treeToSerializedState(tree);
    this.state = decoded ?? defaultState();
    this.render();
  }

  resetLayout(): void {
    this.state = defaultState();
    this.render();
  }

  beginTabDockDrag(): void {
    this.leftZone.el.classList.add("dock-zone-tab-target");
    this.rightZone.el.classList.add("dock-zone-tab-target");
  }

  updateTabDockDrag(clientX: number, clientY: number): void {
    this.leftZone.el.classList.remove("dock-zone-drop");
    this.rightZone.el.classList.remove("dock-zone-drop");
    this.bottomZone.el.classList.remove("dock-zone-drop");
    this.tabDragDropZone = null;

    for (const [zone, zoneEls] of [
      ["left", this.leftZone],
      ["right", this.rightZone],
    ] as const) {
      const rect = zoneEls.el.getBoundingClientRect();
      if (
        clientX >= rect.left &&
        clientX <= rect.right &&
        clientY >= rect.top &&
        clientY <= rect.bottom
      ) {
        this.tabDragDropZone = zone;
        zoneEls.el.classList.add("dock-zone-drop");
        return;
      }
    }
  }

  endTabDockDrag(): DockZone | null {
    const zone = this.tabDragDropZone;
    this.tabDragDropZone = null;
    this.leftZone.el.classList.remove("dock-zone-tab-target");
    this.rightZone.el.classList.remove("dock-zone-tab-target");
    this.leftZone.el.classList.remove("dock-zone-drop");
    this.rightZone.el.classList.remove("dock-zone-drop");
    this.bottomZone.el.classList.remove("dock-zone-drop");
    return zone;
  }

  dockConversationView(
    conversationId: number,
    zone: DockZone,
    viewEl: HTMLElement,
    title: string,
  ): void {
    const wrapperEl = document.createElement("div");
    wrapperEl.className = "docked-conversation";
    wrapperEl.dataset.testid = "docked-conversation";
    wrapperEl.dataset.conversationId = String(conversationId);
    wrapperEl.style.display = "flex";
    wrapperEl.style.flexDirection = "column";
    wrapperEl.style.height = "100%";
    wrapperEl.style.overflow = "hidden";
    wrapperEl.style.minHeight = "0";

    const header = document.createElement("div");
    header.className = "docked-conversation-header";

    const titleEl = document.createElement("span");
    titleEl.className = "docked-conversation-title";
    titleEl.textContent = title;

    header.appendChild(titleEl);

    viewEl.style.flex = "1";
    viewEl.style.overflow = "hidden";
    viewEl.style.minHeight = "0";

    wrapperEl.appendChild(header);
    wrapperEl.appendChild(viewEl);

    this.dockedConversations.set(conversationId, { zone, wrapperEl, title });
    this.activeDockConversation[zone] = conversationId;
    this.activeDockTerminal[zone] = null;
    this.state.activeWidget[zone] = null;

    if (zone === "left") this.state.leftVisible = true;
    if (zone === "right") this.state.rightVisible = true;
    if (zone === "bottom") this.state.bottomVisible = true;

    this.render();
  }

  undockConversationView(conversationId: number): HTMLElement | null {
    const entry = this.dockedConversations.get(conversationId);
    if (!entry) return null;

    const { zone, wrapperEl } = entry;

    const viewEl = wrapperEl.children[1] as HTMLElement | null;
    if (viewEl) {
      viewEl.style.removeProperty("overflow");
      wrapperEl.removeChild(viewEl);
    }
    wrapperEl.remove();

    this.dockedConversations.delete(conversationId);
    if (this.activeDockConversation[zone] === conversationId) {
      let replacement: number | null = null;
      for (const [id, info] of this.dockedConversations) {
        if (info.zone === zone) {
          replacement = id;
          break;
        }
      }
      this.activeDockConversation[zone] = replacement;
      if (!replacement && this.activeDockTerminal[zone] === null) {
        this.state.activeWidget[zone] = this.state.widgetOrder[zone][0] ?? null;
      }
    }

    this.render();
    return viewEl;
  }

  hasDockedConversation(conversationId: number): boolean {
    return this.dockedConversations.has(conversationId);
  }

  activateDockedConversation(conversationId: number): boolean {
    const entry = this.dockedConversations.get(conversationId);
    if (!entry) return false;

    const { zone } = entry;
    this.activeDockConversation[zone] = conversationId;
    this.activeDockTerminal[zone] = null;
    this.state.activeWidget[zone] = null;

    if (zone === "left") this.state.leftVisible = true;
    if (zone === "right") this.state.rightVisible = true;
    if (zone === "bottom") this.state.bottomVisible = true;

    this.render();
    return true;
  }

  getDockedConversationTitle(conversationId: number): string | null {
    return this.dockedConversations.get(conversationId)?.title ?? null;
  }

  dockTerminalView(
    terminalId: number,
    zone: DockZone,
    viewEl: HTMLElement,
    title: string,
  ): void {
    const wrapperEl = document.createElement("div");
    wrapperEl.className = "docked-terminal";
    wrapperEl.dataset.testid = "docked-terminal";
    wrapperEl.dataset.terminalId = String(terminalId);
    wrapperEl.style.display = "flex";
    wrapperEl.style.flexDirection = "column";
    wrapperEl.style.height = "100%";
    wrapperEl.style.overflow = "hidden";
    wrapperEl.style.minHeight = "0";

    viewEl.style.flex = "1";
    viewEl.style.overflow = "hidden";
    viewEl.style.minHeight = "0";
    wrapperEl.appendChild(viewEl);

    this.dockedTerminals.set(terminalId, {
      zone,
      wrapperEl,
      title,
      exited: false,
    });
    this.activeDockTerminal[zone] = terminalId;
    this.activeDockConversation[zone] = null;
    this.state.activeWidget[zone] = null;

    if (zone === "left") this.state.leftVisible = true;
    if (zone === "right") this.state.rightVisible = true;
    if (zone === "bottom") this.state.bottomVisible = true;

    this.render();
    this.onActivateTerminal?.(terminalId);
  }

  removeDockedTerminalView(terminalId: number): HTMLElement | null {
    const entry = this.dockedTerminals.get(terminalId);
    if (!entry) return null;

    const { zone, wrapperEl } = entry;
    const viewEl = wrapperEl.firstElementChild as HTMLElement | null;
    if (viewEl) {
      viewEl.style.removeProperty("overflow");
      wrapperEl.removeChild(viewEl);
    }
    wrapperEl.remove();
    this.dockedTerminals.delete(terminalId);

    if (this.activeDockTerminal[zone] === terminalId) {
      this.activeDockTerminal[zone] = this.firstDockedTerminalInZone(zone);
      if (
        this.activeDockTerminal[zone] === null &&
        this.activeDockConversation[zone] === null
      ) {
        this.state.activeWidget[zone] = this.state.widgetOrder[zone][0] ?? null;
      }
    }

    this.render();
    return viewEl;
  }

  updateDockedTerminalTitle(terminalId: number, title: string): void {
    const entry = this.dockedTerminals.get(terminalId);
    if (!entry) return;
    entry.title = title;
    this.render();
  }

  markDockedTerminalExited(terminalId: number): void {
    const entry = this.dockedTerminals.get(terminalId);
    if (!entry) return;
    entry.exited = true;
    this.render();
  }

  private loadPersistedState(): WorkbenchState | null {
    const raw = localStorage.getItem(this.storageKey);
    if (!raw) return null;
    try {
      const parsed = JSON.parse(raw);
      return sanitizeState(parsed);
    } catch {
      return null;
    }
  }

  private persist(): void {
    localStorage.setItem(this.storageKey, JSON.stringify(this.state));
  }

  private buildDom(panelFactory: PanelFactory): void {
    this.container.innerHTML = "";

    this.rootEl = document.createElement("div");
    this.rootEl.className = "workbench-root";

    this.topEl = document.createElement("div");
    this.topEl.className = "workbench-top";

    this.leftZone = this.createDockZone("left");
    this.rightZone = this.createDockZone("right");
    this.bottomZone = this.createDockZone("bottom");

    this.centerEl = document.createElement("div");
    this.centerEl.className = "center-zone";

    this.centerTabsEl = document.createElement("div");
    this.centerTabsEl.className = "center-tabs";

    this.centerTabsTrackEl = document.createElement("div");
    this.centerTabsTrackEl.className = "center-tabs-track";

    this.centerTabsActionsEl = document.createElement("div");
    this.centerTabsActionsEl.className = "center-tabs-actions";

    this.centerTabsEl.appendChild(this.centerTabsTrackEl);
    this.centerTabsEl.appendChild(this.centerTabsActionsEl);

    this.centerContentEl = document.createElement("div");
    this.centerContentEl.className = "center-content";

    this.chatViewEl = document.createElement("div");
    this.chatViewEl.className = "center-view center-view-chat";
    this.chatShellEl = panelFactory("chat");
    this.chatViewEl.appendChild(this.chatShellEl);

    this.editorViewEl = document.createElement("div");
    this.editorViewEl.className = "center-view center-view-editor";
    this.editorViewEl.appendChild(panelFactory("diff"));

    this.homeViewEl = document.createElement("div");
    this.homeViewEl.className = "center-view center-view-home";

    this.centerContentEl.appendChild(this.chatViewEl);
    this.centerContentEl.appendChild(this.editorViewEl);
    this.centerContentEl.appendChild(this.homeViewEl);

    this.centerEl.appendChild(this.centerTabsEl);
    this.centerEl.appendChild(this.centerContentEl);

    this.leftHandle = document.createElement("div");
    this.leftHandle.className = "workbench-resize-handle workbench-resize-h";

    this.rightHandle = document.createElement("div");
    this.rightHandle.className = "workbench-resize-handle workbench-resize-h";

    this.bottomHandle = document.createElement("div");
    this.bottomHandle.className = "workbench-resize-handle workbench-resize-v";

    this.topEl.appendChild(this.leftZone.el);
    this.topEl.appendChild(this.leftHandle);
    this.topEl.appendChild(this.centerEl);
    this.topEl.appendChild(this.rightHandle);
    this.topEl.appendChild(this.rightZone.el);

    this.rootEl.appendChild(this.topEl);
    this.rootEl.appendChild(this.bottomHandle);
    this.rootEl.appendChild(this.bottomZone.el);
    this.container.appendChild(this.rootEl);

    this.attachResizeHandlers();
  }

  private createDockZone(zone: DockZone): DockZoneElements {
    const el = document.createElement("div");
    el.className = `dock-zone dock-zone-${zone}`;
    el.dataset.zone = zone;

    const tabsEl = document.createElement("div");
    tabsEl.className = "dock-zone-tabs";

    const contentEl = document.createElement("div");
    contentEl.className = "dock-zone-content";

    el.appendChild(tabsEl);
    el.appendChild(contentEl);

    return { el, tabsEl, contentEl };
  }

  private render(): void {
    this.applyCenterView();
    this.renderZone("left", this.leftZone);
    this.renderZone("right", this.rightZone);
    this.renderZone("bottom", this.bottomZone);
    this.applyVisibility();
    this.applyZoneSizes();
    this.persist();
  }

  private applyCenterView(): void {
    this.chatViewEl.classList.toggle(
      "center-view-active",
      this.state.centerView === "chat",
    );
    this.editorViewEl.classList.toggle(
      "center-view-active",
      this.state.centerView === "editor",
    );
    this.homeViewEl.classList.toggle(
      "center-view-active",
      this.state.centerView === "home",
    );
  }

  private renderZone(zone: DockZone, zoneEls: DockZoneElements): void {
    const order = this.getVisibleWidgetOrder(zone);
    const active = this.state.activeWidget[zone];

    if (active && !order.includes(active)) {
      this.state.activeWidget[zone] = order[0] ?? null;
    }

    zoneEls.tabsEl.innerHTML = "";
    for (const widget of order) {
      const tab = document.createElement("div");
      tab.className = "dock-widget-tab";
      tab.dataset.testid = "dock-widget-tab";
      tab.dataset.widget = widget;
      tab.textContent = WIDGET_TITLES[widget];
      tab.classList.toggle(
        "dock-widget-tab-active",
        this.state.activeWidget[zone] === widget,
      );

      tab.addEventListener("click", () => {
        if (this.suppressClickWidget === widget) {
          this.suppressClickWidget = null;
          return;
        }
        this.activeDockConversation[zone] = null;
        this.activeDockTerminal[zone] = null;
        this.state.activeWidget[zone] = widget;
        if (zone === "left") this.state.leftVisible = true;
        if (zone === "right") this.state.rightVisible = true;
        if (zone === "bottom") this.state.bottomVisible = true;
        this.render();
      });

      tab.addEventListener("pointerdown", (e) =>
        this.startWidgetDrag(e, widget),
      );
      zoneEls.tabsEl.appendChild(tab);
    }

    for (const [conversationId, info] of this.dockedConversations) {
      if (info.zone !== zone) continue;

      const tab = document.createElement("div");
      tab.className = "dock-widget-tab";
      tab.dataset.testid = "dock-conversation-tab";
      tab.textContent = info.title;
      tab.classList.toggle(
        "dock-widget-tab-active",
        this.activeDockConversation[zone] === conversationId,
      );

      tab.addEventListener("click", () => {
        if (this.suppressClickConversation === conversationId) {
          this.suppressClickConversation = null;
          return;
        }
        this.activeDockConversation[zone] = conversationId;
        this.activeDockTerminal[zone] = null;
        this.state.activeWidget[zone] = null;
        this.render();
      });

      tab.addEventListener("pointerdown", (e) =>
        this.startConversationDrag(e, conversationId),
      );
      zoneEls.tabsEl.appendChild(tab);
    }

    for (const [terminalId, info] of this.dockedTerminals) {
      if (info.zone !== zone) continue;

      const tab = document.createElement("div");
      tab.className = "dock-widget-tab dock-terminal-tab";
      tab.dataset.testid = "dock-terminal-tab";
      tab.dataset.terminalId = String(terminalId);
      tab.classList.toggle(
        "dock-widget-tab-active",
        this.activeDockTerminal[zone] === terminalId,
      );
      tab.classList.toggle("dock-terminal-tab-exited", info.exited);

      const titleEl = document.createElement("span");
      titleEl.className = "dock-terminal-tab-title";
      titleEl.textContent = info.title;
      titleEl.title = info.title;
      tab.appendChild(titleEl);

      const closeBtn = document.createElement("button");
      closeBtn.type = "button";
      closeBtn.className = "dock-terminal-tab-close";
      closeBtn.textContent = "x";
      closeBtn.title = "Close terminal";
      closeBtn.addEventListener("pointerdown", (event) => {
        event.stopPropagation();
      });
      closeBtn.addEventListener("click", (event) => {
        event.stopPropagation();
        this.onCloseTerminal?.(terminalId);
      });
      tab.appendChild(closeBtn);

      tab.addEventListener("click", () => {
        if (this.suppressClickTerminal === terminalId) {
          this.suppressClickTerminal = null;
          return;
        }
        this.activeDockTerminal[zone] = terminalId;
        this.activeDockConversation[zone] = null;
        this.state.activeWidget[zone] = null;
        this.render();
        this.onActivateTerminal?.(terminalId);
      });

      tab.addEventListener("pointerdown", (e) =>
        this.startTerminalDrag(e, terminalId),
      );
      zoneEls.tabsEl.appendChild(tab);
    }

    if (zone === "bottom" && this.isWidgetAvailable("terminal")) {
      const addBtn = document.createElement("button");
      addBtn.type = "button";
      addBtn.className = "center-tab-new-btn dock-terminal-add-btn";
      addBtn.dataset.testid = "dock-terminal-add-btn";
      addBtn.textContent = "+";
      addBtn.title = "New terminal";
      addBtn.addEventListener("click", () => {
        this.onCreateTerminal?.("bottom");
      });
      zoneEls.tabsEl.appendChild(addBtn);
    }

    let activeConvId = this.activeDockConversation[zone];
    if (activeConvId !== null && !this.dockedConversations.has(activeConvId)) {
      this.activeDockConversation[zone] = null;
      activeConvId = null;
    }

    let activeTerminalId = this.activeDockTerminal[zone];
    if (
      activeTerminalId !== null &&
      !this.dockedTerminals.has(activeTerminalId)
    ) {
      this.activeDockTerminal[zone] = null;
      activeTerminalId = null;
    }
    let desiredContent: HTMLElement | null = null;

    if (activeConvId !== null) {
      const convEntry = this.dockedConversations.get(activeConvId);
      if (convEntry) desiredContent = convEntry.wrapperEl;
    }

    if (!desiredContent && activeTerminalId !== null) {
      const terminalEntry = this.dockedTerminals.get(activeTerminalId);
      if (terminalEntry) desiredContent = terminalEntry.wrapperEl;
    }

    if (!desiredContent) {
      const activeWidget = this.state.activeWidget[zone];
      if (activeWidget) {
        desiredContent = this.widgetPanels.get(activeWidget) ?? null;
      }
    }

    if (!desiredContent) {
      desiredContent = document.createElement("div");
      desiredContent.className = "dock-empty-state";
      desiredContent.textContent = "Drag widgets here";
    }

    if (
      zoneEls.contentEl.firstChild === desiredContent &&
      zoneEls.contentEl.childNodes.length === 1
    )
      return;

    while (zoneEls.contentEl.firstChild) {
      zoneEls.contentEl.removeChild(zoneEls.contentEl.firstChild);
    }
    zoneEls.contentEl.appendChild(desiredContent);
  }

  private applyVisibility(): void {
    const full = this.state.fullScreenChat;
    const home = this.homeMode;

    const leftHidden = full || !this.state.leftVisible;
    this.leftZone.el.classList.toggle("dock-zone-hidden", leftHidden);
    this.leftHandle.classList.toggle("dock-zone-hidden", leftHidden);

    const rightHidden = full || !this.state.rightVisible;
    this.rightZone.el.classList.toggle("dock-zone-hidden", rightHidden);
    this.rightHandle.classList.toggle("dock-zone-hidden", rightHidden);

    const bottomHidden = full || !this.state.bottomVisible;
    this.bottomZone.el.classList.toggle("dock-zone-hidden", bottomHidden);
    this.bottomHandle.classList.toggle("dock-zone-hidden", bottomHidden);
    this.bottomZone.el.classList.remove("dock-zone-collapsed");

    this.centerTabsEl.classList.toggle("dock-zone-hidden", home);
  }

  private applyZoneSizes(): void {
    this.leftZone.el.style.width = `${this.state.leftWidth}px`;
    this.rightZone.el.style.width = `${this.state.rightWidth}px`;
    this.bottomZone.el.style.height = `${this.state.bottomHeight}px`;
  }

  private isWidgetAvailable(widget: PersistentWidgetId): boolean {
    return this.availableWidgets.has(widget);
  }

  private getVisibleWidgetOrder(zone: DockZone): WidgetId[] {
    return this.state.widgetOrder[zone].filter((widget) =>
      this.isWidgetAvailable(widget),
    );
  }

  private attachResizeHandlers(): void {
    this.initHandle(this.leftHandle, "left");
    this.initHandle(this.rightHandle, "right");
    this.initHandle(this.bottomHandle, "bottom");
  }

  private initHandle(
    handle: HTMLElement,
    target: "left" | "right" | "bottom",
  ): void {
    handle.addEventListener("pointerdown", (e) => {
      if (e.button !== 0) return;
      e.preventDefault();
      handle.setPointerCapture(e.pointerId);
      handle.classList.add("resize-active");
      this.resizeTarget = target;
      this.resizeStartPos = target === "bottom" ? e.clientY : e.clientX;
      this.resizeStartSize =
        target === "left"
          ? this.state.leftWidth
          : target === "right"
            ? this.state.rightWidth
            : this.state.bottomHeight;

      const onMove = (me: PointerEvent) => {
        if (this.resizeTarget !== target) return;
        cancelAnimationFrame(this.resizeRafId);
        this.resizeRafId = requestAnimationFrame(() => {
          this.applyResizeDelta(target, me);
        });
      };

      const onUp = () => {
        handle.classList.remove("resize-active");
        this.resizeTarget = null;
        cancelAnimationFrame(this.resizeRafId);
        window.removeEventListener("pointermove", onMove);
        window.removeEventListener("pointerup", onUp);
        this.persist();
      };

      window.addEventListener("pointermove", onMove);
      window.addEventListener("pointerup", onUp);
    });

    handle.addEventListener("dblclick", () => {
      const defaults = defaultState();
      if (target === "left") this.state.leftWidth = defaults.leftWidth;
      else if (target === "right") this.state.rightWidth = defaults.rightWidth;
      else this.state.bottomHeight = defaults.bottomHeight;
      this.render();
    });
  }

  private applyResizeDelta(
    target: "left" | "right" | "bottom",
    e: PointerEvent,
  ): void {
    if (target === "left") {
      const delta = e.clientX - this.resizeStartPos;
      this.state.leftWidth = Math.max(220, this.resizeStartSize + delta);
    } else if (target === "right") {
      const delta = this.resizeStartPos - e.clientX;
      this.state.rightWidth = Math.max(220, this.resizeStartSize + delta);
    } else {
      const delta = this.resizeStartPos - e.clientY;
      this.state.bottomHeight = Math.min(
        400,
        Math.max(100, this.resizeStartSize + delta),
      );
    }
    this.applyZoneSizes();
  }

  private startWidgetDrag(e: PointerEvent, widget: PersistentWidgetId): void {
    if (e.button !== 0) return;
    if (this.draggingWidget !== null) return;

    this.draggingWidget = widget;
    this.draggingPointerId = e.pointerId;
    this.dragStartX = e.clientX;
    this.dragStartY = e.clientY;
    this.dragActive = false;
    this.dropZone = null;

    const target = e.target as HTMLElement;
    if (target.setPointerCapture) {
      target.setPointerCapture(e.pointerId);
    }

    this.pointerMoveHandler = (evt: PointerEvent) =>
      this.handleWidgetDragMove(evt);
    this.pointerUpHandler = (evt: PointerEvent) =>
      this.handleWidgetDragEnd(evt);
    window.addEventListener("pointermove", this.pointerMoveHandler);
    window.addEventListener("pointerup", this.pointerUpHandler);
    window.addEventListener("pointercancel", this.pointerUpHandler);
  }

  private handleWidgetDragMove(e: PointerEvent): void {
    if (!this.draggingWidget) return;
    if (
      this.draggingPointerId !== null &&
      e.pointerId !== this.draggingPointerId
    )
      return;

    const dx = e.clientX - this.dragStartX;
    const dy = e.clientY - this.dragStartY;

    if (!this.dragActive) {
      if (Math.hypot(dx, dy) < 6) return;
      this.dragActive = true;
      this.setWidgetDropTargetsVisible(true);
      this.markDraggingWidgetTab();
    }

    e.preventDefault();
    this.updateDropZone(e.clientX, e.clientY);
  }

  private markDraggingWidgetTab(): void {
    if (!this.draggingWidget) return;
    for (const zoneEls of [this.leftZone, this.rightZone, this.bottomZone]) {
      const tab = zoneEls.tabsEl.querySelector<HTMLElement>(
        `.dock-widget-tab[data-widget="${this.draggingWidget}"]`,
      );
      tab?.classList.add("dock-widget-tab-dragging");
    }
  }

  private setWidgetDropTargetsVisible(visible: boolean): void {
    for (const zoneEls of [this.leftZone, this.rightZone, this.bottomZone]) {
      zoneEls.el.classList.toggle("dock-zone-tab-target", visible);
    }
  }

  private clearDragIndicators(): void {
    this.removeWidgetDropOverlays();
    for (const zoneEls of [this.leftZone, this.rightZone, this.bottomZone]) {
      zoneEls.el.classList.remove("dock-zone-drop");
      zoneEls.tabsEl.querySelectorAll(".dock-widget-tab").forEach((el) => {
        el.classList.remove("dock-widget-tab-dragging");
      });
    }
  }

  private updateDropZone(clientX: number, clientY: number): void {
    this.clearDragIndicators();
    this.dropZone = null;

    for (const [zone, zoneEls] of [
      ["left", this.leftZone],
      ["right", this.rightZone],
      ["bottom", this.bottomZone],
    ] as const) {
      const rect = zoneEls.el.getBoundingClientRect();
      if (
        clientX >= rect.left &&
        clientX <= rect.right &&
        clientY >= rect.top &&
        clientY <= rect.bottom
      ) {
        this.dropZone = zone;
        zoneEls.el.classList.add("dock-zone-drop");
        this.addWidgetDropOverlay(zoneEls.el);
        this.markDraggingWidgetTab();
        return;
      }
    }

    this.markDraggingWidgetTab();
  }

  private addWidgetDropOverlay(zoneEl: HTMLElement): void {
    this.removeWidgetDropOverlays();
    const rect = zoneEl.getBoundingClientRect();
    const overlay = document.createElement("div");
    overlay.className = "dock-drop-overlay";
    overlay.style.position = "fixed";
    overlay.style.left = `${rect.left}px`;
    overlay.style.top = `${rect.top}px`;
    overlay.style.width = `${rect.width}px`;
    overlay.style.height = `${rect.height}px`;
    document.body.appendChild(overlay);
    this.widgetDropOverlays.push(overlay);
  }

  private removeWidgetDropOverlays(): void {
    for (const overlay of this.widgetDropOverlays) {
      overlay.remove();
    }
    this.widgetDropOverlays = [];
  }

  private handleWidgetDragEnd(e: PointerEvent): void {
    if (!this.draggingWidget) return;
    if (
      this.draggingPointerId !== null &&
      e.pointerId !== this.draggingPointerId
    )
      return;

    const widget = this.draggingWidget;
    const wasDrag = this.dragActive;
    const dropZone = this.dropZone;
    this.cleanupWidgetDrag();

    if (!wasDrag || !dropZone) return;
    this.suppressClickWidget = widget;

    this.moveWidget(widget, dropZone);
  }

  private cleanupWidgetDrag(): void {
    if (this.pointerMoveHandler) {
      window.removeEventListener("pointermove", this.pointerMoveHandler);
      this.pointerMoveHandler = null;
    }
    if (this.pointerUpHandler) {
      window.removeEventListener("pointerup", this.pointerUpHandler);
      window.removeEventListener("pointercancel", this.pointerUpHandler);
      this.pointerUpHandler = null;
    }

    this.draggingWidget = null;
    this.draggingPointerId = null;
    this.dragStartX = 0;
    this.dragStartY = 0;
    this.dragActive = false;
    this.dropZone = null;
    this.setWidgetDropTargetsVisible(false);
    this.clearDragIndicators();
  }

  private startConversationDrag(e: PointerEvent, conversationId: number): void {
    if (e.button !== 0) return;
    if (this.draggingConversation !== null) return;

    this.draggingConversation = conversationId;
    this.draggingConvPointerId = e.pointerId;
    this.convDragStartX = e.clientX;
    this.convDragStartY = e.clientY;
    this.convDragActive = false;
    this.convDropOnCenter = false;

    const target = e.target as HTMLElement;
    if (target.setPointerCapture) {
      target.setPointerCapture(e.pointerId);
    }

    this.convPointerMoveHandler = (evt: PointerEvent) =>
      this.handleConvDragMove(evt);
    this.convPointerUpHandler = (evt: PointerEvent) =>
      this.handleConvDragEnd(evt);
    window.addEventListener("pointermove", this.convPointerMoveHandler);
    window.addEventListener("pointerup", this.convPointerUpHandler);
    window.addEventListener("pointercancel", this.convPointerUpHandler);
  }

  private handleConvDragMove(e: PointerEvent): void {
    if (this.draggingConversation === null) return;
    if (
      this.draggingConvPointerId !== null &&
      e.pointerId !== this.draggingConvPointerId
    )
      return;

    const dx = e.clientX - this.convDragStartX;
    const dy = e.clientY - this.convDragStartY;

    if (!this.convDragActive) {
      if (Math.hypot(dx, dy) < 6) return;
      this.convDragActive = true;
      this.markDraggingConversationTab();
    }

    e.preventDefault();

    const hovered = document.elementFromPoint(
      e.clientX,
      e.clientY,
    ) as HTMLElement | null;
    this.convDropOnCenter = false;
    if (hovered) {
      const centerZone =
        hovered.closest(".center-zone") || hovered.closest(".conv-tab-bar");
      if (centerZone) {
        this.convDropOnCenter = true;
        this.centerEl.classList.add("center-zone-drop-target");
      }
    }
    if (!this.convDropOnCenter) {
      this.centerEl.classList.remove("center-zone-drop-target");
    }
  }

  private handleConvDragEnd(e: PointerEvent): void {
    if (this.draggingConversation === null) return;
    if (
      this.draggingConvPointerId !== null &&
      e.pointerId !== this.draggingConvPointerId
    )
      return;

    const conversationId = this.draggingConversation;
    const wasActive = this.convDragActive;
    const wasDropOnCenter = this.convDropOnCenter;
    this.cleanupConvDrag();

    if (!wasActive) return;
    this.suppressClickConversation = conversationId;

    if (wasDropOnCenter) {
      this.onUndockConversation?.(conversationId);
    }
  }

  private cleanupConvDrag(): void {
    if (this.convPointerMoveHandler) {
      window.removeEventListener("pointermove", this.convPointerMoveHandler);
      this.convPointerMoveHandler = null;
    }
    if (this.convPointerUpHandler) {
      window.removeEventListener("pointerup", this.convPointerUpHandler);
      window.removeEventListener("pointercancel", this.convPointerUpHandler);
      this.convPointerUpHandler = null;
    }

    this.draggingConversation = null;
    this.draggingConvPointerId = null;
    this.convDragStartX = 0;
    this.convDragStartY = 0;
    this.convDragActive = false;
    this.convDropOnCenter = false;
    this.centerEl.classList.remove("center-zone-drop-target");

    for (const zoneEls of [this.leftZone, this.rightZone, this.bottomZone]) {
      zoneEls.tabsEl.querySelectorAll(".dock-widget-tab").forEach((el) => {
        el.classList.remove("dock-widget-tab-dragging");
      });
    }
  }

  private markDraggingConversationTab(): void {
    if (this.draggingConversation === null) return;
    for (const zoneEls of [this.leftZone, this.rightZone, this.bottomZone]) {
      zoneEls.tabsEl
        .querySelectorAll<HTMLElement>('[data-testid="dock-conversation-tab"]')
        .forEach((tab) => {
          if (
            tab.textContent ===
            this.dockedConversations.get(this.draggingConversation!)?.title
          ) {
            tab.classList.add("dock-widget-tab-dragging");
          }
        });
    }
  }

  private startTerminalDrag(e: PointerEvent, terminalId: number): void {
    if (e.button !== 0) return;
    if (this.draggingTerminal !== null) return;

    this.draggingTerminal = terminalId;
    this.draggingTerminalPointerId = e.pointerId;
    this.terminalDragStartX = e.clientX;
    this.terminalDragStartY = e.clientY;
    this.terminalDragActive = false;
    this.dropZone = null;

    const target = e.target as HTMLElement;
    if (target.setPointerCapture) {
      target.setPointerCapture(e.pointerId);
    }

    this.terminalPointerMoveHandler = (evt: PointerEvent) =>
      this.handleTerminalDragMove(evt);
    this.terminalPointerUpHandler = (evt: PointerEvent) =>
      this.handleTerminalDragEnd(evt);
    window.addEventListener("pointermove", this.terminalPointerMoveHandler);
    window.addEventListener("pointerup", this.terminalPointerUpHandler);
    window.addEventListener("pointercancel", this.terminalPointerUpHandler);
  }

  private handleTerminalDragMove(e: PointerEvent): void {
    if (this.draggingTerminal === null) return;
    if (
      this.draggingTerminalPointerId !== null &&
      e.pointerId !== this.draggingTerminalPointerId
    )
      return;

    const dx = e.clientX - this.terminalDragStartX;
    const dy = e.clientY - this.terminalDragStartY;

    if (!this.terminalDragActive) {
      if (Math.hypot(dx, dy) < 6) return;
      this.terminalDragActive = true;
      this.setWidgetDropTargetsVisible(true);
      this.markDraggingTerminalTab();
    }

    e.preventDefault();
    this.updateDropZone(e.clientX, e.clientY);
    this.markDraggingTerminalTab();
  }

  private handleTerminalDragEnd(e: PointerEvent): void {
    if (this.draggingTerminal === null) return;
    if (
      this.draggingTerminalPointerId !== null &&
      e.pointerId !== this.draggingTerminalPointerId
    )
      return;

    const terminalId = this.draggingTerminal;
    const wasDrag = this.terminalDragActive;
    const dropZone = this.dropZone;
    this.cleanupTerminalDrag();

    if (!wasDrag || !dropZone) return;
    this.suppressClickTerminal = terminalId;
    this.moveDockedTerminal(terminalId, dropZone);
  }

  private cleanupTerminalDrag(): void {
    if (this.terminalPointerMoveHandler) {
      window.removeEventListener(
        "pointermove",
        this.terminalPointerMoveHandler,
      );
      this.terminalPointerMoveHandler = null;
    }
    if (this.terminalPointerUpHandler) {
      window.removeEventListener("pointerup", this.terminalPointerUpHandler);
      window.removeEventListener(
        "pointercancel",
        this.terminalPointerUpHandler,
      );
      this.terminalPointerUpHandler = null;
    }

    this.draggingTerminal = null;
    this.draggingTerminalPointerId = null;
    this.terminalDragStartX = 0;
    this.terminalDragStartY = 0;
    this.terminalDragActive = false;
    this.dropZone = null;
    this.setWidgetDropTargetsVisible(false);
    this.clearDragIndicators();
  }

  private markDraggingTerminalTab(): void {
    if (this.draggingTerminal === null) return;
    for (const zoneEls of [this.leftZone, this.rightZone, this.bottomZone]) {
      const tab = zoneEls.tabsEl.querySelector<HTMLElement>(
        `.dock-terminal-tab[data-terminal-id="${this.draggingTerminal}"]`,
      );
      tab?.classList.add("dock-widget-tab-dragging");
    }
  }

  private firstDockedTerminalInZone(zone: DockZone): number | null {
    for (const [id, info] of this.dockedTerminals) {
      if (info.zone === zone) return id;
    }
    return null;
  }

  private moveDockedTerminal(terminalId: number, targetZone: DockZone): void {
    const entry = this.dockedTerminals.get(terminalId);
    if (!entry) return;
    const sourceZone = entry.zone;

    if (sourceZone === targetZone) {
      this.activeDockTerminal[targetZone] = terminalId;
      this.activeDockConversation[targetZone] = null;
      this.state.activeWidget[targetZone] = null;
      if (targetZone === "left") this.state.leftVisible = true;
      if (targetZone === "right") this.state.rightVisible = true;
      if (targetZone === "bottom") this.state.bottomVisible = true;
      this.render();
      this.onActivateTerminal?.(terminalId);
      return;
    }

    entry.zone = targetZone;

    if (this.activeDockTerminal[sourceZone] === terminalId) {
      this.activeDockTerminal[sourceZone] =
        this.firstDockedTerminalInZone(sourceZone);
      if (
        this.activeDockTerminal[sourceZone] === null &&
        this.activeDockConversation[sourceZone] === null
      ) {
        this.state.activeWidget[sourceZone] =
          this.state.widgetOrder[sourceZone][0] ?? null;
      }
    }

    this.activeDockTerminal[targetZone] = terminalId;
    this.activeDockConversation[targetZone] = null;
    this.state.activeWidget[targetZone] = null;

    if (targetZone === "left") this.state.leftVisible = true;
    if (targetZone === "right") this.state.rightVisible = true;
    if (targetZone === "bottom") this.state.bottomVisible = true;

    this.render();
    this.onActivateTerminal?.(terminalId);
  }

  private moveWidget(widget: PersistentWidgetId, targetZone: DockZone): void {
    const sourceZone = this.state.widgetZones[widget];

    if (sourceZone === targetZone) {
      this.activeDockConversation[targetZone] = null;
      this.activeDockTerminal[targetZone] = null;
      this.state.activeWidget[targetZone] = widget;
      if (targetZone === "left") this.state.leftVisible = true;
      if (targetZone === "right") this.state.rightVisible = true;
      if (targetZone === "bottom") this.state.bottomVisible = true;
      this.render();
      return;
    }

    this.state.widgetOrder[sourceZone] = this.state.widgetOrder[
      sourceZone
    ].filter((w) => w !== widget);
    if (this.state.activeWidget[sourceZone] === widget) {
      this.state.activeWidget[sourceZone] =
        this.state.widgetOrder[sourceZone][0] ?? null;
    }

    this.state.widgetZones[widget] = targetZone;
    this.state.widgetOrder[targetZone] = this.state.widgetOrder[
      targetZone
    ].filter((w) => w !== widget);
    this.state.widgetOrder[targetZone].push(widget);
    this.activeDockConversation[targetZone] = null;
    this.activeDockTerminal[targetZone] = null;
    this.state.activeWidget[targetZone] = widget;

    if (targetZone === "left") this.state.leftVisible = true;
    if (targetZone === "right") this.state.rightVisible = true;
    if (targetZone === "bottom") this.state.bottomVisible = true;

    this.render();
  }
}
