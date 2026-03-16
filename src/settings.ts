import {
  adminGetModuleSchemas,
  adminGetSettings,
  adminListProfiles,
  adminSwitchProfile,
  adminUpdateSettings,
  type BackendDependencyStatus,
  type BackendDepResult,
  type BackendKind,
  checkBackendDependencies as checkBackendDependenciesBridge,
  type DriverMcpHttpServerSettings,
  getDriverMcpHttpServerSettings as getDriverMcpHttpServerSettingsBridge,
  getMcpHttpServerSettings as getMcpHttpServerSettingsBridge,
  installBackendDependency as installBackendDependencyBridge,
  type McpHttpServerSettings,
  setDisabledBackends as setDisabledBackendsBridge,
  setDriverMcpHttpServerAutoloadEnabled as setDriverMcpHttpServerAutoloadEnabledBridge,
  setDriverMcpHttpServerEnabled as setDriverMcpHttpServerEnabledBridge,
  setMcpHttpServerEnabled as setMcpHttpServerEnabledBridge,
} from "./bridge";
import {
  broadcastToolOutputMode,
  getToolOutputMode,
  onToolOutputModeChange,
  setToolOutputMode,
  type ToolOutputMode,
} from "./chat/tools";

const APPEARANCE_STORAGE_KEY = "tyde-appearance";
const ACTIVE_SETTINGS_TAB_KEY = "tyde-settings-active-tab";
const DEFAULT_SPAWN_PROFILE_STORAGE_KEY = "tyde-default-spawn-profile";
const DEFAULT_BACKEND_STORAGE_KEY = "tyde-default-backend";

const VALID_THEME = ["system", "dark", "light"] as const;
type ThemeMode = (typeof VALID_THEME)[number];

interface AppearanceSettings {
  theme: ThemeMode;
  fontSize: number;
}

// --- Appearance persistence (localStorage only, not sent to subprocess) ---

function loadAppearanceSettings(): AppearanceSettings {
  const raw = localStorage.getItem(APPEARANCE_STORAGE_KEY);
  if (!raw) return { theme: "system", fontSize: 14 };
  try {
    const parsed = JSON.parse(raw);
    const theme = VALID_THEME.includes(parsed.theme) ? parsed.theme : "system";
    return { theme, fontSize: clampFontSize(Number(parsed.fontSize ?? 14)) };
  } catch (e) {
    console.error("Corrupt appearance settings, resetting:", e);
    localStorage.removeItem(APPEARANCE_STORAGE_KEY);
    return { theme: "system", fontSize: 14 };
  }
}

function saveAppearanceSettings(s: AppearanceSettings): void {
  localStorage.setItem(APPEARANCE_STORAGE_KEY, JSON.stringify(s));
}

function clampFontSize(v: number): number {
  if (!Number.isFinite(v)) return 14;
  return Math.min(20, Math.max(11, Math.round(v)));
}

export function adjustFontSize(delta: number): void {
  const current = loadAppearanceSettings();
  current.fontSize = clampFontSize(current.fontSize + delta);
  saveAppearanceSettings(current);
  applyAppearanceToDocument(current);

  const slider = document.querySelector(
    ".settings-base-font-slider",
  ) as HTMLInputElement | null;
  if (slider) slider.value = String(current.fontSize);
}

function applyAppearanceToDocument(a: AppearanceSettings): void {
  const root = document.documentElement;
  root.style.setProperty("--base-font-size", `${clampFontSize(a.fontSize)}px`);

  if (a.theme === "system") {
    localStorage.removeItem("tyde-theme");
    const prefersDark = window.matchMedia(
      "(prefers-color-scheme: dark)",
    ).matches;
    root.dataset.theme = prefersDark ? "" : "light";
    return;
  }
  localStorage.setItem("tyde-theme", a.theme);
  root.dataset.theme = a.theme === "light" ? "light" : "";
}

// --- Tab ID management ---

type SettingsTabId = string;

function loadActiveTab(): SettingsTabId {
  const stored = localStorage.getItem(ACTIVE_SETTINGS_TAB_KEY) ?? "appearance";
  if (stored.startsWith("codex")) return "general";
  return stored;
}

function saveActiveTab(tab: SettingsTabId): void {
  localStorage.setItem(ACTIVE_SETTINGS_TAB_KEY, tab);
}

function normalizeProfileName(value: string | null): string | null {
  if (value === null) return null;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
}

function normalizeBackendKind(value: string | null | undefined): BackendKind {
  const normalized = (value ?? "").trim().toLowerCase();
  if (normalized === "codex") return "codex";
  if (normalized === "claude" || normalized === "claude_code") return "claude";
  if (normalized === "kiro") return "kiro";
  return "tycode";
}

export function getDefaultBackend(): BackendKind {
  try {
    return normalizeBackendKind(
      localStorage.getItem(DEFAULT_BACKEND_STORAGE_KEY),
    );
  } catch (err) {
    console.error("Failed to read default backend from localStorage:", err);
    return "tycode";
  }
}

export function setDefaultBackend(backend: string | null | undefined): void {
  try {
    localStorage.setItem(
      DEFAULT_BACKEND_STORAGE_KEY,
      normalizeBackendKind(backend),
    );
  } catch (err) {
    console.error("Failed to save default backend to localStorage:", err);
  }
}

export function getDefaultSpawnProfile(): string | null {
  try {
    return normalizeProfileName(
      localStorage.getItem(DEFAULT_SPAWN_PROFILE_STORAGE_KEY),
    );
  } catch (err) {
    console.error(
      "Failed to read default spawn profile from localStorage:",
      err,
    );
    return null;
  }
}

export function setDefaultSpawnProfile(profileName: string | null): void {
  try {
    const normalized = normalizeProfileName(profileName);
    if (normalized === null) {
      localStorage.removeItem(DEFAULT_SPAWN_PROFILE_STORAGE_KEY);
      return;
    }
    localStorage.setItem(DEFAULT_SPAWN_PROFILE_STORAGE_KEY, normalized);
  } catch (err) {
    console.error("Failed to save default spawn profile to localStorage:", err);
  }
}

// --- Backend enable/disable persistence ---

const ENABLED_BACKENDS_STORAGE_KEY = "tyde-enabled-backends";
const ALL_BACKENDS: BackendKind[] = ["tycode", "codex", "claude", "kiro"];

let cachedDependencyStatus: Record<BackendKind, BackendDepResult> | null = null;

export function setCachedDependencyStatus(
  status: BackendDependencyStatus,
): void {
  cachedDependencyStatus = {
    tycode: status.tycode,
    codex: status.codex,
    claude: status.claude,
    kiro: status.kiro,
  };
}

export function getCachedDependencyStatus(): Record<
  BackendKind,
  BackendDepResult
> | null {
  return cachedDependencyStatus;
}

export function getEnabledBackendPreferences(): BackendKind[] {
  const raw = localStorage.getItem(ENABLED_BACKENDS_STORAGE_KEY);
  if (!raw) return [...ALL_BACKENDS];
  try {
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [...ALL_BACKENDS];
    return parsed.filter((b: string) =>
      (ALL_BACKENDS as string[]).includes(b),
    ) as BackendKind[];
  } catch {
    return [...ALL_BACKENDS];
  }
}

export function setEnabledBackendPreferences(backends: BackendKind[]): void {
  localStorage.setItem(ENABLED_BACKENDS_STORAGE_KEY, JSON.stringify(backends));
}

export function isBackendEnabled(kind: BackendKind): boolean {
  const prefs = getEnabledBackendPreferences();
  if (!prefs.includes(kind)) return false;
  if (cachedDependencyStatus && !cachedDependencyStatus[kind].available)
    return false;
  return true;
}

export function getEnabledBackends(): BackendKind[] {
  return ALL_BACKENDS.filter(isBackendEnabled);
}

function syncDisabledBackendsToRust(): void {
  const disabled = ALL_BACKENDS.filter((b) => !isBackendEnabled(b));
  setDisabledBackendsBridge(disabled).catch((err) => {
    console.error("Failed to sync disabled backends to Rust:", err);
  });
}

export async function initializeBackendDependencies(): Promise<void> {
  try {
    const status = await checkBackendDependenciesBridge();
    setCachedDependencyStatus(status);
    syncDisabledBackendsToRust();
  } catch (err) {
    console.error("Failed to initialize backend dependencies:", err);
  }
}

// --- Schema resolution helpers (ported from VSCode settings.js) ---

function resolveSchemaRef(fieldSchema: any, rootSchema: any): any {
  if (!fieldSchema) return fieldSchema;

  if (fieldSchema.$ref) {
    const refPath: string = fieldSchema.$ref;
    if (!refPath.startsWith("#/definitions/")) return fieldSchema;
    const typeName = refPath.substring("#/definitions/".length);
    const resolved = rootSchema?.definitions?.[typeName];
    return resolved ?? fieldSchema;
  }

  // allOf with $ref — schemars pattern for fields with defaults
  if (Array.isArray(fieldSchema.allOf)) {
    for (const item of fieldSchema.allOf) {
      if (!item.$ref) continue;
      const refPath: string = item.$ref;
      if (!refPath.startsWith("#/definitions/")) continue;
      const typeName = refPath.substring("#/definitions/".length);
      const resolved = rootSchema?.definitions?.[typeName];
      if (!resolved) continue;
      const merged = { ...fieldSchema, ...resolved };
      delete merged.allOf;
      return merged;
    }
  }
  return fieldSchema;
}

function isNumberType(opt: any, rootSchema: any): boolean {
  if (opt.type === "integer" || opt.type === "number") return true;
  if (!opt.$ref || !rootSchema?.definitions) return false;
  const refPath: string = opt.$ref;
  if (!refPath.startsWith("#/definitions/")) return false;
  const typeName = refPath.substring("#/definitions/".length);
  const resolved = rootSchema.definitions[typeName];
  return resolved?.type === "integer" || resolved?.type === "number";
}

function isNullableNumber(schema: any, rootSchema: any): boolean {
  if (!schema) return false;

  if (Array.isArray(schema.type)) {
    const hasNum =
      schema.type.includes("integer") || schema.type.includes("number");
    if (hasNum && schema.type.includes("null")) return true;
  }

  for (const key of ["anyOf", "oneOf"] as const) {
    const arr = schema[key];
    if (!Array.isArray(arr)) continue;
    const hasNum = arr.some((s: any) => isNumberType(s, rootSchema));
    const hasNull = arr.some((s: any) => s.type === "null");
    if (hasNum && hasNull) return true;
  }
  return false;
}

// --- HTML helpers ---

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs?: Record<string, string>,
  text?: string,
): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  if (attrs) {
    for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, v);
  }
  if (text !== undefined) e.textContent = text;
  return e;
}

// --- General tab field definitions ---

interface SelectFieldDef {
  key: string;
  label: string;
  description: string;
  options: string[];
  humanLabels?: Record<string, string>;
  aliases?: Record<string, string>;
  nullable?: boolean;
  noneLabel?: string;
}

interface ProviderEntry {
  name: string;
  config: Record<string, any>;
  index: number | null;
  key: string | null;
}

interface McpEntry {
  name: string;
  config: Record<string, any>;
  index: number | null;
  key: string | null;
}

const GENERAL_FIELDS: SelectFieldDef[] = [
  {
    key: "review_level",
    label: "Review Level",
    description: "Adds human-like review checks before execution.",
    options: ["None", "Task"],
    aliases: {
      Basic: "Task",
      Thorough: "Task",
    },
  },
  {
    key: "model_quality",
    label: "Model Quality",
    description: "Cost ceiling for model selection.",
    options: ["free", "low", "medium", "high", "unlimited"],
    humanLabels: {
      free: "Free",
      low: "Low",
      medium: "Medium",
      high: "High",
      unlimited: "Unlimited",
    },
    aliases: {
      Free: "free",
      Low: "low",
      Medium: "medium",
      High: "high",
      Unlimited: "unlimited",
      Fast: "low",
      Standard: "medium",
      Premium: "high",
    },
    nullable: true,
    noneLabel: "Provider default",
  },
  {
    key: "reasoning_effort",
    label: "Reasoning Effort",
    description: "More effort yields slower but better planning.",
    options: ["Off", "Low", "Medium", "High", "Max"],
    nullable: true,
    noneLabel: "Agent default",
  },
  {
    key: "communication_tone",
    label: "Communication Tone",
    description: "Controls verbosity of agent responses.",
    options: ["concise_and_logical", "warm_and_flowy", "cat", "meme"],
    humanLabels: {
      concise_and_logical: "Concise and logical",
      warm_and_flowy: "Warm and flowy",
      cat: "Cat",
      meme: "Meme",
    },
    aliases: {
      Normal: "concise_and_logical",
      Terse: "concise_and_logical",
      Verbose: "warm_and_flowy",
      ConciseAndLogical: "concise_and_logical",
      WarmAndFlowy: "warm_and_flowy",
    },
  },
  {
    key: "autonomy_level",
    label: "Autonomy Level",
    description: "Plan approval required or fully autonomous execution.",
    options: ["plan_approval_required", "fully_autonomous"],
    humanLabels: {
      plan_approval_required: "Plan approval required",
      fully_autonomous: "Fully autonomous",
    },
    aliases: {
      PlanApproval: "plan_approval_required",
      FullyAutonomous: "fully_autonomous",
    },
  },
  {
    key: "default_agent",
    label: "Default Agent",
    description: "Agent to use when starting new conversations.",
    options: [
      "tycode",
      "one_shot",
      "coder",
      "context",
      "debugger",
      "planner",
      "coordinator",
    ],
  },
];

// --- Main class ---

export class SettingsPanel {
  onClose: (() => void) | null = null;
  onMcpHttpSettingsChange: ((settings: McpHttpServerSettings) => void) | null =
    null;
  onBackendsChanged: (() => void) | null = null;

  private container: HTMLElement;
  private appearance: AppearanceSettings;
  private backendSettings: Record<string, any> = {};
  private mcpHttpServerSettings: McpHttpServerSettings = {
    enabled: true,
    running: false,
    url: null,
  };
  private mcpHttpStatusLoading = false;
  private mcpHttpStatusError: string | null = null;
  private driverMcpHttpServerSettings: DriverMcpHttpServerSettings = {
    enabled: false,
    autoload: false,
    running: false,
    url: null,
  };
  private driverMcpHttpStatusLoading = false;
  private driverMcpHttpStatusError: string | null = null;
  private moduleSchemas: any[] = [];
  private profiles: string[] = [];
  private activeProfile: string | null = null;
  private defaultBackend: BackendKind = getDefaultBackend();
  private defaultSpawnProfile: string | null = getDefaultSpawnProfile();
  private activeTab: SettingsTabId;
  private searchQuery = "";
  private backendDependencyStatus: Record<
    BackendKind,
    BackendDepResult
  > | null = null;
  private installingBackends: Set<BackendKind> = new Set();
  private backendInstallError: Map<BackendKind, string> = new Map();
  private _adminId: number | null = null;

  get adminId(): number | null {
    return this._adminId;
  }

  set adminId(id: number | null) {
    if (id === this._adminId) return;
    this._adminId = id;
    if (id === null) return;
    adminGetSettings(id).catch((err) =>
      console.error("Failed to get admin settings:", err),
    );
    adminGetModuleSchemas(id).catch((err) =>
      console.error("Failed to get admin module schemas:", err),
    );
    adminListProfiles(id).catch((err) =>
      console.error("Failed to list admin profiles:", err),
    );
  }

  constructor(container: HTMLElement) {
    this.container = container;
    this.appearance = loadAppearanceSettings();
    this.activeTab = loadActiveTab();
    applyAppearanceToDocument(this.appearance);
    this.refreshMcpHttpServerSettings();
    this.refreshDriverMcpHttpServerSettings();
    this.refreshBackendDependencies();
    this.render();
  }

  // --- Public handlers called by event_router.ts ---

  handleSettingsData(data: any): void {
    if (!data || typeof data !== "object") return;
    this.backendSettings = data;
    const profile = data.profile ?? data.active_profile;
    if (typeof profile === "string" && profile.length > 0) {
      this.activeProfile = profile;
      this.syncProfileDropdown();
    }
    this.rerenderBackendTabs();
  }

  handleModuleSchemas(schemas: any[]): void {
    if (!Array.isArray(schemas)) return;
    this.moduleSchemas = schemas;
    this.renderModuleTabs();
  }

  handleProfilesList(data: any): void {
    if (!data || typeof data !== "object") return;
    if (Array.isArray(data.profiles)) this.profiles = data.profiles;
    const profile = data.selectedProfile ?? data.active_profile;
    if (typeof profile === "string" && profile.length > 0) {
      this.activeProfile = profile;
    }
    this.syncProfileDropdown();
  }

  refreshMcpHttpServerSettings(): void {
    this.mcpHttpStatusLoading = true;
    this.mcpHttpStatusError = null;
    getMcpHttpServerSettingsBridge()
      .then((settings) => {
        this.mcpHttpServerSettings = settings;
        this.onMcpHttpSettingsChange?.(settings);
      })
      .catch((err) => {
        this.mcpHttpStatusError =
          err instanceof Error ? err.message : String(err);
        console.error("Failed to load MCP HTTP server settings:", err);
      })
      .finally(() => {
        this.mcpHttpStatusLoading = false;
        if (this.activeTab === "tyde") {
          this.rerenderPanelContent("tyde", () => this.buildTydeContent());
        }
      });
  }

  refreshDriverMcpHttpServerSettings(): void {
    this.driverMcpHttpStatusLoading = true;
    this.driverMcpHttpStatusError = null;
    getDriverMcpHttpServerSettingsBridge()
      .then((settings) => {
        this.driverMcpHttpServerSettings = settings;
      })
      .catch((err) => {
        this.driverMcpHttpStatusError =
          err instanceof Error ? err.message : String(err);
        console.error("Failed to load driver MCP HTTP server settings:", err);
      })
      .finally(() => {
        this.driverMcpHttpStatusLoading = false;
        if (this.activeTab === "tyde") {
          this.rerenderPanelContent("tyde", () => this.buildTydeContent());
        }
      });
  }

  refreshBackendDependencies(): void {
    checkBackendDependenciesBridge()
      .then((status) => {
        this.backendDependencyStatus = {
          tycode: status.tycode,
          codex: status.codex,
          claude: status.claude,
          kiro: status.kiro,
        };
        setCachedDependencyStatus(status);
        syncDisabledBackendsToRust();
        if (this.activeTab === "backends") {
          this.rerenderPanelContent("backends", () =>
            this.buildBackendsContent(),
          );
        }
        this.syncProfileDropdown();
        this.onBackendsChanged?.();
      })
      .catch((err) => {
        console.error("Failed to check backend dependencies:", err);
      });
  }

  // --- Persistence ---

  private persistToBackend(): void {
    if (this.adminId === null) return;
    adminUpdateSettings(this.adminId, this.backendSettings).catch((err) => {
      console.error("Failed to persist settings to backend:", err);
    });
  }

  // --- Render ---

  private render(): void {
    this.container.innerHTML = "";
    const panel = el("div", {
      class: "settings-panel settings-root",
      role: "tabpanel",
    });
    panel.dataset.testid = "settings-panel";

    const closeBtn = el(
      "button",
      { class: "settings-close-btn", "aria-label": "Close settings" },
      "×",
    );
    closeBtn.dataset.testid = "settings-close";
    closeBtn.addEventListener("click", () => this.onClose?.());
    panel.appendChild(closeBtn);

    const layout = el("div", { class: "settings-layout" });
    const nav = el("nav", {
      class: "settings-nav",
      role: "tablist",
      "aria-label": "Settings categories",
    });
    nav.dataset.testid = "settings-nav";
    const content = el("div", { class: "settings-content" });

    // Search box
    const searchWrap = el("div", { class: "settings-search-wrap" });
    const searchInput = el("input", {
      class: "settings-search-input",
      type: "text",
      placeholder: "Search settings...",
      "aria-label": "Search settings",
    });
    searchInput.addEventListener("input", () =>
      this.filterSettings(searchInput.value),
    );
    searchWrap.appendChild(searchInput);
    nav.appendChild(searchWrap);

    const uiExpanded =
      this.activeTab === "appearance" ||
      this.activeTab === "backends" ||
      this.activeTab === "tyde";
    const aiExpanded = !uiExpanded;

    // Tyde Settings collapsible group
    const uiGroup = this.buildNavGroup("Tyde Settings", uiExpanded, "ui");
    const uiItems = uiGroup.querySelector(".settings-nav-group-items")!;
    const appearanceBtn = el(
      "button",
      {
        class: "nav-item",
        "data-tab": "appearance",
        role: "tab",
        "data-testid": "settings-nav-item",
      },
      "Appearance",
    );
    appearanceBtn.addEventListener("click", () => this.switchTab("appearance"));
    uiItems.appendChild(appearanceBtn);
    const backendsBtn = el(
      "button",
      {
        class: "nav-item",
        "data-tab": "backends",
        role: "tab",
        "data-testid": "settings-nav-item",
      },
      "Backends",
    );
    backendsBtn.addEventListener("click", () => this.switchTab("backends"));
    uiItems.appendChild(backendsBtn);
    const tydeBtn = el(
      "button",
      {
        class: "nav-item",
        "data-tab": "tyde",
        role: "tab",
        "data-testid": "settings-nav-item",
      },
      "Agent Control",
    );
    tydeBtn.addEventListener("click", () => this.switchTab("tyde"));
    uiItems.appendChild(tydeBtn);
    nav.appendChild(uiGroup);

    // Tycode Settings collapsible group
    const aiGroup = this.buildNavGroup("Tycode Settings", aiExpanded, "ai");
    const aiItems = aiGroup.querySelector(".settings-nav-group-items")!;
    aiItems.appendChild(this.buildProfileSection());
    const aiTabs: [string, string][] = [
      ["general", "General"],
      ["providers", "Providers"],
      ["mcp", "MCP Servers"],
      ["agent-models", "Agent Models"],
      ["advanced", "Advanced"],
    ];
    for (const [id, label] of aiTabs) {
      const btn = el(
        "button",
        {
          class: "nav-item",
          "data-tab": id,
          role: "tab",
          "data-testid": "settings-nav-item",
        },
        label,
      );
      btn.addEventListener("click", () => this.switchTab(id));
      aiItems.appendChild(btn);
    }
    nav.appendChild(aiGroup);

    // Tab panels
    content.appendChild(this.buildAppearancePanel());
    content.appendChild(this.buildBackendsPanel());
    content.appendChild(this.buildTydePanel());
    content.appendChild(this.buildGeneralPanel());
    content.appendChild(this.buildProvidersPanel());
    content.appendChild(this.buildMcpPanel());
    content.appendChild(this.buildAgentModelsPanel());
    content.appendChild(this.buildAdvancedPanel());

    layout.appendChild(nav);
    layout.appendChild(content);
    panel.appendChild(layout);
    this.container.appendChild(panel);

    this.syncTabVisibility();
    this.syncAppearanceUI();
    this.syncProfileDropdown();
  }

  // Build collapsible nav group
  private buildNavGroup(
    title: string,
    expanded: boolean,
    groupId: "ui" | "ai",
  ): HTMLElement {
    const group = el("div", {
      class: "settings-nav-group",
      "data-group": groupId,
    });
    if (expanded) group.classList.add("expanded");

    const header = el("button", {
      class: "settings-nav-group-header",
      "aria-expanded": String(expanded),
    });
    header.appendChild(
      el("span", { class: "settings-nav-group-chevron" }, expanded ? "▼" : "▶"),
    );
    header.appendChild(
      el("span", { class: "settings-nav-group-title" }, title),
    );
    header.addEventListener("click", () => {
      const isExpanded = group.classList.toggle("expanded");
      header.setAttribute("aria-expanded", String(isExpanded));
      header.querySelector(".settings-nav-group-chevron")!.textContent =
        isExpanded ? "▼" : "▶";
    });
    group.appendChild(header);

    const items = el("div", { class: "settings-nav-group-items" });
    if (expanded) items.classList.add("expanded");
    group.appendChild(items);

    return group;
  }

  private setNavGroupExpanded(groupId: "ui" | "ai", expanded: boolean): void {
    const group = this.container.querySelector(
      `.settings-nav-group[data-group="${groupId}"]`,
    ) as HTMLElement | null;
    if (!group) return;
    group.classList.toggle("expanded", expanded);
    const header = group.querySelector(
      ".settings-nav-group-header",
    ) as HTMLElement | null;
    if (!header) return;
    header.setAttribute("aria-expanded", String(expanded));
    const chevron = header.querySelector(".settings-nav-group-chevron");
    if (chevron) chevron.textContent = expanded ? "▼" : "▶";
  }

  private syncNavGroupsForActiveTab(): void {
    const uiActive =
      this.activeTab === "appearance" ||
      this.activeTab === "backends" ||
      this.activeTab === "tyde";
    this.setNavGroupExpanded("ui", uiActive);
    this.setNavGroupExpanded("ai", !uiActive);
  }

  private resetSearchFilters(): void {
    for (const item of this.container.querySelectorAll(
      ".nav-item, .tab-panel, .settings-field, .settings-card, .settings-provider-card",
    )) {
      (item as HTMLElement).style.display = "";
    }
  }

  // Filter settings by search query
  private filterSettings(query: string): void {
    const q = query.toLowerCase().trim();
    this.searchQuery = q;
    const content = this.container.querySelector(".settings-content");
    if (!content) return;

    if (!q) {
      this.resetSearchFilters();
      this.syncTabVisibility();
      return;
    }

    const panelMatches = new Map<string, boolean>();

    for (const panelEl of content.querySelectorAll(".tab-panel")) {
      const panel = panelEl as HTMLElement;
      const panelId = panel.dataset.panel ?? "";
      let hasMatch = false;

      const title =
        panel.querySelector(".tab-title")?.textContent?.toLowerCase() ?? "";
      if (title.includes(q)) hasMatch = true;

      for (const fieldEl of panel.querySelectorAll(".settings-field")) {
        const field = fieldEl as HTMLElement;
        const label =
          field.querySelector(".settings-label")?.textContent?.toLowerCase() ??
          "";
        const desc =
          field
            .querySelector(".settings-description")
            ?.textContent?.toLowerCase() ?? "";
        const match = label.includes(q) || desc.includes(q);
        field.style.display = match ? "" : "none";
        if (match) hasMatch = true;
      }

      for (const cardEl of panel.querySelectorAll(
        ".settings-card, .settings-provider-card",
      )) {
        const card = cardEl as HTMLElement;
        const name = (
          card.querySelector(".settings-card-name")?.textContent ??
          card.querySelector(".settings-provider-name")?.textContent ??
          ""
        ).toLowerCase();
        const detail = (
          card.querySelector(".settings-card-detail")?.textContent ??
          card.querySelector(".settings-provider-detail")?.textContent ??
          ""
        ).toLowerCase();
        const match = name.includes(q) || detail.includes(q);
        card.style.display = match ? "" : "none";
        if (match) hasMatch = true;
      }

      panelMatches.set(panelId, hasMatch);
      panel.style.display = hasMatch ? "block" : "none";
    }

    for (const navItemEl of this.container.querySelectorAll(".nav-item")) {
      const navItem = navItemEl as HTMLElement;
      const label = navItem.textContent?.toLowerCase() ?? "";
      const tab = navItem.dataset.tab ?? "";
      const matchByLabel = label.includes(q);
      const matchByPanel = panelMatches.get(tab) === true;
      navItem.style.display = matchByLabel || matchByPanel ? "" : "none";
    }
  }

  // --- Tab switching ---

  private switchTab(tab: SettingsTabId): void {
    if (this.searchQuery.length > 0) {
      const searchInput = this.container.querySelector(
        ".settings-search-input",
      ) as HTMLInputElement | null;
      if (searchInput) searchInput.value = "";
      this.searchQuery = "";
      this.resetSearchFilters();
    }
    this.activeTab = tab;
    saveActiveTab(tab);
    this.syncTabVisibility();
  }

  private syncTabVisibility(): void {
    this.syncNavGroupsForActiveTab();

    for (const navEl of this.container.querySelectorAll(".nav-item")) {
      const t = (navEl as HTMLElement).dataset.tab;
      const active = t === this.activeTab;
      navEl.classList.toggle("active", active);
      navEl.setAttribute("aria-selected", String(active));
    }

    const searching = this.searchQuery.length > 0;
    for (const panelEl of this.container.querySelectorAll(".tab-panel")) {
      const t = (panelEl as HTMLElement).dataset.panel;
      panelEl.classList.toggle("active", t === this.activeTab);
      if (!searching) (panelEl as HTMLElement).style.display = "";
    }
  }

  // --- Re-render backend-dependent tabs in place ---

  private rerenderBackendTabs(): void {
    this.rerenderPanelContent("tyde", () => this.buildTydeContent());
    this.rerenderPanelContent("general", () => this.buildGeneralContent());
    this.rerenderPanelContent("providers", () => this.buildProvidersContent());
    this.rerenderPanelContent("mcp", () => this.buildMcpContent());
    this.rerenderPanelContent("agent-models", () =>
      this.buildAgentModelsContent(),
    );
    this.rerenderPanelContent("advanced", () => this.buildAdvancedContent());
    if (this.searchQuery.length > 0) {
      this.filterSettings(this.searchQuery);
    }
  }

  private rerenderPanelContent(
    panelId: string,
    builder: () => DocumentFragment,
  ): void {
    const panel = this.container.querySelector(
      `.tab-panel[data-panel="${panelId}"]`,
    );
    if (!panel) return;
    // Keep the h2 title, replace rest
    const title = panel.querySelector(".tab-title");
    panel.innerHTML = "";
    if (title) panel.appendChild(title);
    panel.appendChild(builder());
  }

  // ========== APPEARANCE TAB ==========

  private buildAppearancePanel(): HTMLElement {
    const section = el("section", {
      class: "tab-panel",
      "data-panel": "appearance",
      role: "tabpanel",
      "data-testid": "settings-tab-panel",
    });
    section.appendChild(el("h2", { class: "tab-title" }, "Appearance"));

    // Theme
    const themeSection = el("div", { class: "settings-section" });
    themeSection.appendChild(
      el("h3", { class: "settings-section-header" }, "Theme"),
    );

    const themeField = el("div", { class: "settings-field" });
    themeField.appendChild(
      el(
        "label",
        { class: "settings-label", "data-testid": "settings-label" },
        "Color Scheme",
      ),
    );
    themeField.appendChild(
      el(
        "p",
        { class: "settings-description" },
        "Choose light, dark, or follow system preference.",
      ),
    );

    const themeControl = el("div", {
      class: "settings-segmented-control",
      "data-setting": "theme",
      role: "radiogroup",
    });
    for (const v of ["system", "dark", "light"]) {
      const btn = el(
        "button",
        {
          class: "segment",
          "data-value": v,
          role: "radio",
          "aria-checked": "false",
        },
        v.charAt(0).toUpperCase() + v.slice(1),
      );
      btn.addEventListener("click", () => {
        if (!(VALID_THEME as readonly string[]).includes(v)) return;
        this.appearance.theme = v as ThemeMode;
        saveAppearanceSettings(this.appearance);
        applyAppearanceToDocument(this.appearance);
        this.setActiveSegment(themeControl, v);
      });
      themeControl.appendChild(btn);
    }
    themeField.appendChild(themeControl);
    themeSection.appendChild(themeField);
    section.appendChild(themeSection);

    // Font size
    const typoSection = el("div", { class: "settings-section" });
    typoSection.appendChild(
      el("h3", { class: "settings-section-header" }, "Typography"),
    );

    const fontField = el("div", { class: "settings-field" });
    fontField.appendChild(
      el(
        "label",
        { class: "settings-label", "data-testid": "settings-label" },
        "Font Size",
      ),
    );
    fontField.appendChild(
      el(
        "p",
        { class: "settings-description" },
        "Adjust base typography for the entire IDE.",
      ),
    );

    const fontRow = el("div", { class: "settings-font-row" });
    fontRow.appendChild(
      el("span", { class: "settings-font-scale-label" }, "Smaller"),
    );
    const slider = el("input", {
      class: "settings-font-slider settings-base-font-slider",
      type: "range",
      min: "11",
      max: "20",
      step: "1",
    });
    slider.addEventListener("input", () => {
      this.appearance.fontSize = clampFontSize(Number(slider.value));
      saveAppearanceSettings(this.appearance);
      applyAppearanceToDocument(this.appearance);
    });
    fontRow.appendChild(slider);
    fontRow.appendChild(
      el("span", { class: "settings-font-scale-label" }, "Larger"),
    );
    fontField.appendChild(fontRow);
    typoSection.appendChild(fontField);
    section.appendChild(typoSection);

    // Tool Output
    const outputSection = el("div", { class: "settings-section" });
    outputSection.appendChild(
      el("h3", { class: "settings-section-header" }, "Tool Output"),
    );

    const outputField = el("div", { class: "settings-field" });
    outputField.appendChild(
      el(
        "label",
        { class: "settings-label", "data-testid": "settings-label" },
        "Tool Output Detail",
      ),
    );
    outputField.appendChild(
      el(
        "p",
        { class: "settings-description" },
        "Control how much tool output is shown in chat cards.",
      ),
    );

    const outputControl = el("div", {
      class: "settings-segmented-control",
      "data-setting": "tool-output-mode",
      role: "radiogroup",
    });
    const outputModes: [ToolOutputMode, string][] = [
      ["summary", "Summary"],
      ["compact", "Compact"],
      ["verbose", "Verbose"],
    ];
    for (const [mode, label] of outputModes) {
      const btn = el(
        "button",
        {
          class: "segment",
          "data-value": mode,
          role: "radio",
          "aria-checked": "false",
        },
        label,
      );
      btn.addEventListener("click", () => {
        setToolOutputMode(mode);
        broadcastToolOutputMode();
        this.setActiveSegment(outputControl, mode);
      });
      outputControl.appendChild(btn);
    }
    onToolOutputModeChange((mode) =>
      this.setActiveSegment(outputControl, mode),
    );
    outputField.appendChild(outputControl);
    outputSection.appendChild(outputField);
    section.appendChild(outputSection);

    return section;
  }

  private syncAppearanceUI(): void {
    const themeControl = this.container.querySelector(
      '.settings-segmented-control[data-setting="theme"]',
    ) as HTMLElement | null;
    if (themeControl)
      this.setActiveSegment(themeControl, this.appearance.theme);

    const slider = this.container.querySelector(
      ".settings-base-font-slider",
    ) as HTMLInputElement | null;
    if (slider) slider.value = String(this.appearance.fontSize);

    const outputControl = this.container.querySelector(
      '.settings-segmented-control[data-setting="tool-output-mode"]',
    ) as HTMLElement | null;
    if (outputControl)
      this.setActiveSegment(outputControl, getToolOutputMode());
  }

  // ========== BACKENDS TAB ==========

  private buildBackendsPanel(): HTMLElement {
    const section = el("section", {
      class: "tab-panel",
      "data-panel": "backends",
      role: "tabpanel",
      "data-testid": "settings-tab-panel",
    });
    section.appendChild(el("h2", { class: "tab-title" }, "Backends"));
    section.appendChild(this.buildBackendsContent());
    return section;
  }

  private buildBackendsContent(): DocumentFragment {
    const frag = document.createDocumentFragment();
    frag.appendChild(this.buildDefaultBackendSection());
    const enabledPrefs = getEnabledBackendPreferences();

    const backends: { kind: BackendKind; label: string; binary: string }[] = [
      { kind: "tycode", label: "Tycode", binary: "tycode-subprocess" },
      { kind: "codex", label: "Codex", binary: "codex" },
      { kind: "claude", label: "Claude Code", binary: "claude" },
      { kind: "kiro", label: "Kiro", binary: "kiro-cli" },
    ];

    for (const { kind, label, binary } of backends) {
      const dep = this.backendDependencyStatus?.[kind];
      const depMissing = dep !== undefined && !dep.available;

      const section = el("div", { class: "settings-section" });
      const field = el("div", { class: "settings-field" });
      const row = el("div", { class: "settings-toggle-row" });
      const labelCol = el("div", { class: "settings-toggle-label-col" });

      labelCol.appendChild(
        el(
          "label",
          { class: "settings-label", "data-testid": "settings-label" },
          label,
        ),
      );

      if (depMissing) {
        labelCol.appendChild(
          el(
            "p",
            { class: "settings-description settings-backend-warning" },
            `"${binary}" was not found in PATH. Install it to enable this backend.`,
          ),
        );

        const installing = this.installingBackends.has(kind);
        const installError = this.backendInstallError.get(kind);

        const installBtn = el("button", {
          class: "settings-install-btn",
          "data-testid": `settings-backend-${kind}-install`,
        }) as HTMLButtonElement;
        installBtn.textContent = installing ? "Installing..." : "Install";
        installBtn.disabled = installing;
        installBtn.addEventListener("click", () => {
          this.installingBackends.add(kind);
          this.backendInstallError.delete(kind);
          this.rerenderPanelContent("backends", () =>
            this.buildBackendsContent(),
          );
          installBackendDependencyBridge(kind)
            .then(() => {
              this.installingBackends.delete(kind);
              this.refreshBackendDependencies();
            })
            .catch((err) => {
              this.installingBackends.delete(kind);
              this.backendInstallError.set(kind, String(err));
              this.rerenderPanelContent("backends", () =>
                this.buildBackendsContent(),
              );
            });
        });
        labelCol.appendChild(installBtn);

        if (installError) {
          labelCol.appendChild(
            el(
              "p",
              { class: "settings-description settings-backend-warning" },
              installError,
            ),
          );
        }
      } else {
        labelCol.appendChild(
          el(
            "p",
            { class: "settings-description" },
            `Enable or disable the ${label} backend.`,
          ),
        );
      }

      row.appendChild(labelCol);

      const toggle = el("label", { class: "settings-toggle" });
      const input = el("input", {
        type: "checkbox",
        "data-testid": `settings-backend-${kind}-enabled`,
      }) as HTMLInputElement;
      input.checked = enabledPrefs.includes(kind) && !depMissing;
      input.disabled = depMissing;
      input.addEventListener("change", () => {
        const current = getEnabledBackendPreferences();
        if (input.checked) {
          if (!current.includes(kind)) current.push(kind);
        } else {
          const idx = current.indexOf(kind);
          if (idx !== -1) current.splice(idx, 1);
        }
        setEnabledBackendPreferences(current);
        syncDisabledBackendsToRust();
        this.syncProfileDropdown();
        this.onBackendsChanged?.();
      });
      toggle.appendChild(input);
      toggle.appendChild(el("span", { class: "settings-toggle-slider" }));
      row.appendChild(toggle);

      field.appendChild(row);
      section.appendChild(field);
      frag.appendChild(section);
    }

    return frag;
  }

  // ========== GENERAL TAB ==========

  private buildGeneralPanel(): HTMLElement {
    const section = el("section", {
      class: "tab-panel",
      "data-panel": "general",
      role: "tabpanel",
      "data-testid": "settings-tab-panel",
    });
    section.appendChild(el("h2", { class: "tab-title" }, "General"));
    section.appendChild(this.buildGeneralContent());
    return section;
  }

  private buildGeneralContent(): DocumentFragment {
    const frag = document.createDocumentFragment();
    const section = el("div", { class: "settings-section" });
    const orderedKeys = [
      "model_quality",
      "review_level",
      "reasoning_effort",
      "communication_tone",
      "autonomy_level",
      "default_agent",
    ];
    for (const key of orderedKeys) {
      const field = GENERAL_FIELDS.find((f) => f.key === key);
      if (field) section.appendChild(this.buildSelectField(field));
    }
    frag.appendChild(section);

    return frag;
  }

  private normalizeSelectValue(field: SelectFieldDef, value: unknown): string {
    if (value === null || value === undefined) return "";
    if (typeof value !== "string" || value.length === 0) return "";
    if (field.options.includes(value)) return value;

    // Case-insensitive direct option match.
    const lower = value.toLowerCase();
    const optionMatch = field.options.find(
      (opt) => opt.toLowerCase() === lower,
    );
    if (optionMatch) return optionMatch;

    if (field.aliases) {
      if (value in field.aliases) return field.aliases[value];
      const aliasMatch = Object.entries(field.aliases).find(
        ([alias]) => alias.toLowerCase() === lower,
      );
      if (aliasMatch) return aliasMatch[1];
    }

    return value;
  }

  // Build a select field with label, description, and human-readable options
  private buildSelectField(
    field: SelectFieldDef,
    includeDescription = true,
  ): HTMLElement {
    const fieldEl = el("div", { class: "settings-field" });
    const labelRow = el("div", { class: "settings-field-header" });
    labelRow.appendChild(
      el(
        "label",
        { class: "settings-label", "data-testid": "settings-label" },
        field.label,
      ),
    );
    fieldEl.appendChild(labelRow);
    if (includeDescription && field.description) {
      fieldEl.appendChild(
        el("p", { class: "settings-description" }, field.description),
      );
    }
    const select = el("select", {
      class: "settings-select",
      "data-testid": "settings-select",
    });
    const currentVal = this.normalizeSelectValue(
      field,
      this.backendSettings[field.key],
    );
    if (currentVal && this.backendSettings[field.key] !== currentVal) {
      this.backendSettings[field.key] = currentVal;
    }
    if (field.nullable) {
      const noneLabel = field.noneLabel ?? "Default";
      const emptyOption = el("option", { value: "" }, noneLabel);
      if (!currentVal) emptyOption.selected = true;
      select.appendChild(emptyOption);
    }

    for (const opt of field.options) {
      const label = field.humanLabels?.[opt] ?? opt;
      const option = el("option", { value: opt }, label);
      if (opt === currentVal) option.selected = true;
      select.appendChild(option);
    }
    if (currentVal && !field.options.includes(currentVal)) {
      const fallback = el("option", { value: currentVal }, currentVal);
      fallback.selected = true;
      select.appendChild(fallback);
    }
    select.addEventListener("change", () => {
      if (field.nullable && select.value === "") {
        this.backendSettings[field.key] = null;
      } else {
        this.backendSettings[field.key] = select.value;
      }
      this.persistToBackend();
    });
    fieldEl.appendChild(select);
    return fieldEl;
  }

  // ========== PROVIDERS TAB ==========

  private buildProvidersPanel(): HTMLElement {
    const section = el("section", {
      class: "tab-panel",
      "data-panel": "providers",
      role: "tabpanel",
      "data-testid": "settings-tab-panel",
    });
    section.appendChild(el("h2", { class: "tab-title" }, "Providers"));
    section.appendChild(this.buildProvidersContent());
    return section;
  }

  private buildProvidersContent(): DocumentFragment {
    const frag = document.createDocumentFragment();
    const list = el("div", { class: "settings-card-list" });

    for (const entry of this.getProviderEntries()) {
      list.appendChild(this.buildProviderCard(entry));
    }

    const addBtn = el(
      "button",
      { class: "settings-add-btn settings-add-btn-primary" },
      "+ Add Provider",
    );
    addBtn.addEventListener("click", () => this.showProviderModal(null));

    frag.appendChild(list);
    frag.appendChild(addBtn);
    return frag;
  }

  private getProviderEntries(): ProviderEntry[] {
    const providers = this.backendSettings.providers;
    if (Array.isArray(providers)) {
      return providers
        .map((config, index): ProviderEntry | null => {
          if (!config || typeof config !== "object") return null;
          const explicitName =
            typeof config.name === "string" ? config.name.trim() : "";
          const fallbackName = `Provider ${index + 1}`;
          return {
            name: explicitName || fallbackName,
            config: config as Record<string, any>,
            index,
            key: null,
          };
        })
        .filter((entry): entry is ProviderEntry => entry !== null);
    }
    if (providers && typeof providers === "object") {
      return Object.entries(providers as Record<string, any>).map(
        ([name, config]) => ({
          name,
          config: (config && typeof config === "object"
            ? config
            : {}) as Record<string, any>,
          index: null,
          key: name,
        }),
      );
    }
    return [];
  }

  private upsertProviderEntry(
    existing: ProviderEntry | null,
    nextName: string,
    nextConfig: Record<string, any>,
  ): void {
    if (
      Array.isArray(this.backendSettings.providers) ||
      this.backendSettings.providers === undefined
    ) {
      if (!Array.isArray(this.backendSettings.providers))
        this.backendSettings.providers = [];
      const providers = this.backendSettings.providers as Record<string, any>[];
      const row = { ...nextConfig, name: nextName };
      if (
        existing?.index !== null &&
        existing?.index !== undefined &&
        existing.index >= 0 &&
        existing.index < providers.length
      ) {
        providers[existing.index] = row;
      } else {
        providers.push(row);
      }
      return;
    }

    if (
      !this.backendSettings.providers ||
      typeof this.backendSettings.providers !== "object"
    ) {
      this.backendSettings.providers = {};
    }
    const providers = this.backendSettings.providers as Record<
      string,
      Record<string, any>
    >;
    if (existing?.key && existing.key !== nextName) {
      delete providers[existing.key];
    }
    providers[nextName] = nextConfig;
  }

  private deleteProviderEntry(entry: ProviderEntry): void {
    if (Array.isArray(this.backendSettings.providers)) {
      const providers = this.backendSettings.providers as any[];
      if (
        entry.index !== null &&
        entry.index >= 0 &&
        entry.index < providers.length
      ) {
        providers.splice(entry.index, 1);
        return;
      }
      const indexByName = providers.findIndex(
        (p) => p && typeof p === "object" && p.name === entry.name,
      );
      if (indexByName >= 0) providers.splice(indexByName, 1);
      return;
    }
    if (
      !this.backendSettings.providers ||
      typeof this.backendSettings.providers !== "object"
    )
      return;
    const providers = this.backendSettings.providers as Record<string, any>;
    if (entry.key && entry.key in providers) {
      delete providers[entry.key];
      return;
    }
    if (entry.name in providers) delete providers[entry.name];
  }

  private getProviderType(provider: Record<string, any>): string {
    const rawType = provider.type ?? provider.provider;
    if (typeof rawType !== "string" || rawType.length === 0) return "unknown";
    return rawType;
  }

  private buildProviderCard(entry: ProviderEntry): HTMLElement {
    const { name, config: provider } = entry;
    const card = el("div", { class: "settings-provider-card" });
    const providerType = this.getProviderType(provider);

    // Determine status
    let status: "connected" | "missing_key" | "error" = "connected";
    if (provider.error) status = "error";
    if (providerType === "openrouter" && !provider.api_key)
      status = "missing_key";
    if (providerType === "bedrock" && !provider.profile) status = "missing_key";

    const statusLabels: Record<string, string> = {
      connected: "Connected",
      missing_key: "Missing config",
      error: "Error",
    };

    // Info section (left side) - contains header + detail
    const info = el("div", { class: "settings-provider-info" });

    // Header row with name + status
    const header = el("div", { class: "settings-provider-header" });
    const nameEl = el(
      "span",
      { class: "settings-provider-name", "data-testid": "settings-card-name" },
      name,
    );
    const statusChip = el(
      "span",
      { class: `settings-status-chip status-${status}` },
      statusLabels[status],
    );
    header.appendChild(nameEl);
    header.appendChild(statusChip);
    info.appendChild(header);

    // Detail row
    const detail: string[] = [];
    detail.push(`Type: ${providerType}`);
    if (providerType === "bedrock") {
      if (provider.profile) detail.push(`Profile: ${provider.profile}`);
      if (provider.region) detail.push(`Region: ${provider.region}`);
    }
    if (providerType === "openrouter" && provider.api_key) {
      detail.push("API key configured");
    }
    if (
      (providerType === "claude_code" || providerType === "codex") &&
      provider.command
    ) {
      detail.push(`Cmd: ${provider.command}`);
    }
    if (Array.isArray(provider.extra_args) && provider.extra_args.length > 0) {
      detail.push(`Args: ${provider.extra_args.length}`);
    }
    if (provider.model) detail.push(`Model: ${provider.model}`);

    const detailText = detail.filter(Boolean).join(" · ");
    if (detailText)
      info.appendChild(
        el("div", { class: "settings-provider-detail" }, detailText),
      );

    if (provider.base_url || provider.endpoint) {
      const endpoint = provider.base_url ?? provider.endpoint;
      const endpointRow = el("div", { class: "settings-provider-expanded" });
      endpointRow.appendChild(el("span", {}, `Endpoint: ${endpoint}`));
      info.appendChild(endpointRow);
    }

    card.appendChild(info);

    // Actions section (right side)
    const actions = el("div", { class: "settings-provider-actions" });
    const editBtn = el(
      "button",
      { class: "settings-action-btn", title: "Edit provider" },
      "Edit",
    );
    editBtn.addEventListener("click", () => this.showProviderModal(entry));
    const delBtn = el(
      "button",
      {
        class: "settings-action-btn settings-provider-delete",
        title: "Delete provider",
      },
      "Delete",
    );
    delBtn.addEventListener("click", () => this.confirmDeleteProvider(entry));
    actions.appendChild(editBtn);
    actions.appendChild(delBtn);
    card.appendChild(actions);

    return card;
  }

  private confirmDeleteProvider(entry: ProviderEntry): void {
    const overlay = el("div", { class: "settings-modal-overlay" });
    const modal = el("div", { class: "settings-modal settings-confirm-modal" });
    modal.appendChild(el("h3", {}, "Delete Provider"));
    modal.appendChild(
      el(
        "p",
        {},
        `Are you sure you want to delete "${entry.name}"? This action cannot be undone.`,
      ),
    );

    const actions = el("div", { class: "settings-modal-actions" });
    const cancelBtn = el("button", {}, "Cancel");
    cancelBtn.addEventListener("click", () => overlay.remove());
    const deleteBtn = el(
      "button",
      { class: "settings-modal-delete" },
      "Delete",
    );
    deleteBtn.addEventListener("click", () => {
      this.deleteProviderEntry(entry);
      this.persistToBackend();
      this.rerenderPanelContent("providers", () =>
        this.buildProvidersContent(),
      );
      overlay.remove();
    });
    actions.appendChild(cancelBtn);
    actions.appendChild(deleteBtn);
    modal.appendChild(actions);

    overlay.appendChild(modal);
    overlay.addEventListener("click", (e) => {
      if (e.target === overlay) overlay.remove();
    });
    document.body.appendChild(overlay);
  }

  private showProviderModal(existing: ProviderEntry | null): void {
    const isEdit = existing !== null;
    const cfg = existing?.config ?? {};
    const currentName = existing?.name ?? "";
    const initialType = this.getProviderType(cfg);

    const overlay = el("div", { class: "settings-modal-overlay" });
    const modal = el("div", { class: "settings-modal" });
    modal.appendChild(el("h3", {}, isEdit ? "Edit Provider" : "Add Provider"));

    const nameField = el("div", { class: "settings-field" });
    nameField.appendChild(
      el(
        "label",
        { class: "settings-label", "data-testid": "settings-label" },
        "Name",
      ),
    );
    const nameInput = el("input", {
      class: "settings-input",
      type: "text",
      value: currentName,
    });
    nameField.appendChild(nameInput);
    modal.appendChild(nameField);

    const typeField = el("div", { class: "settings-field" });
    typeField.appendChild(
      el(
        "label",
        { class: "settings-label", "data-testid": "settings-label" },
        "Provider Type",
      ),
    );
    const typeSelect = el("select", {
      class: "settings-select",
      "data-testid": "settings-select",
    });
    const providerTypes: [string, string][] = [
      ["openrouter", "OpenRouter"],
      ["bedrock", "AWS Bedrock"],
      ["claude_code", "Claude Code"],
      ["codex", "Codex CLI"],
      ["mock", "Mock"],
    ];
    for (const [value, label] of providerTypes) {
      const opt = el("option", { value }, label);
      if (value === initialType) opt.selected = true;
      typeSelect.appendChild(opt);
    }
    if (typeSelect.value === "" && initialType && initialType !== "unknown") {
      const legacy = el(
        "option",
        { value: initialType },
        `${initialType} (Legacy)`,
      );
      legacy.selected = true;
      typeSelect.appendChild(legacy);
    }
    if (typeSelect.value === "") typeSelect.value = "openrouter";
    typeField.appendChild(typeSelect);
    modal.appendChild(typeField);

    const dynamicFields = el("div");
    modal.appendChild(dynamicFields);

    const envToLines = (envObj: any): string => {
      if (!envObj || typeof envObj !== "object") return "";
      return Object.entries(envObj)
        .map(([k, v]) => `${k}=${String(v)}`)
        .join("\n");
    };

    const parseLines = (value: string): string[] => {
      return value
        .split("\n")
        .map((line) => line.trim())
        .filter(Boolean);
    };

    const parseEnvLines = (value: string): Record<string, string> => {
      const env: Record<string, string> = {};
      for (const line of value.split("\n")) {
        const trimmed = line.trim();
        if (!trimmed) continue;
        const eq = trimmed.indexOf("=");
        if (eq <= 0) continue;
        const k = trimmed.slice(0, eq).trim();
        const v = trimmed.slice(eq + 1).trim();
        if (k) env[k] = v;
      }
      return env;
    };

    const createInputField = (
      labelText: string,
      value: string,
    ): HTMLInputElement => {
      const field = el("div", { class: "settings-field" });
      field.appendChild(
        el(
          "label",
          { class: "settings-label", "data-testid": "settings-label" },
          labelText,
        ),
      );
      const input = el("input", {
        class: "settings-input",
        type: "text",
        value,
      });
      field.appendChild(input);
      dynamicFields.appendChild(field);
      return input as HTMLInputElement;
    };

    const createTextareaField = (
      labelText: string,
      value: string,
    ): HTMLTextAreaElement => {
      const field = el("div", { class: "settings-field" });
      field.appendChild(
        el(
          "label",
          { class: "settings-label", "data-testid": "settings-label" },
          labelText,
        ),
      );
      const textarea = el("textarea", {
        class: "settings-textarea",
        rows: "3",
      });
      textarea.value = value;
      field.appendChild(textarea);
      dynamicFields.appendChild(field);
      return textarea as HTMLTextAreaElement;
    };

    let profileInput: HTMLInputElement | null = null;
    let regionInput: HTMLInputElement | null = null;
    let apiInput: HTMLInputElement | null = null;
    let commandInput: HTMLInputElement | null = null;
    let argsArea: HTMLTextAreaElement | null = null;
    let envArea: HTMLTextAreaElement | null = null;

    const renderDynamicFields = (): void => {
      dynamicFields.innerHTML = "";
      const providerType = typeSelect.value;
      profileInput = null;
      regionInput = null;
      apiInput = null;
      commandInput = null;
      argsArea = null;
      envArea = null;

      if (providerType === "bedrock") {
        profileInput = createInputField(
          "AWS Profile",
          String(cfg.profile ?? "default"),
        );
        regionInput = createInputField(
          "Region",
          String(cfg.region ?? "us-west-2"),
        );
      } else if (providerType === "openrouter") {
        apiInput = createInputField("API Key", String(cfg.api_key ?? ""));
      } else if (providerType === "claude_code" || providerType === "codex") {
        const fallbackCommand =
          providerType === "claude_code" ? "claude" : "codex";
        commandInput = createInputField(
          "Command",
          String(cfg.command ?? fallbackCommand),
        );
        argsArea = createTextareaField(
          "Extra Args (one per line)",
          Array.isArray(cfg.extra_args) ? cfg.extra_args.join("\n") : "",
        );
        envArea = createTextareaField(
          "Env Vars (KEY=VALUE per line)",
          envToLines(cfg.env),
        );
      }
    };

    renderDynamicFields();
    typeSelect.addEventListener("change", renderDynamicFields);

    const actions = el("div", { class: "settings-modal-actions" });
    const cancelBtn = el("button", {}, "Cancel");
    cancelBtn.addEventListener("click", () => overlay.remove());
    const saveBtn = el("button", { class: "settings-modal-save" }, "Save");
    saveBtn.addEventListener("click", () => {
      const name = nameInput.value.trim();
      if (!name) return;
      const type = typeSelect.value;
      const obj: Record<string, any> = { type };

      if (type === "bedrock") {
        const profile = profileInput?.value.trim();
        const region = regionInput?.value.trim();
        if (profile) obj.profile = profile;
        obj.region = region || "us-west-2";
      } else if (type === "openrouter") {
        const apiKey = apiInput?.value.trim();
        if (apiKey) obj.api_key = apiKey;
      } else if (type === "claude_code" || type === "codex") {
        const command = commandInput?.value.trim();
        const extraArgs = parseLines(argsArea?.value ?? "");
        const env = parseEnvLines(envArea?.value ?? "");
        if (command) obj.command = command;
        if (extraArgs.length > 0) obj.extra_args = extraArgs;
        if (Object.keys(env).length > 0) obj.env = env;
      } else if (type === "mock") {
        if (cfg.behavior !== undefined) obj.behavior = cfg.behavior;
      }

      this.upsertProviderEntry(existing, name, obj);
      this.persistToBackend();
      this.rerenderPanelContent("providers", () =>
        this.buildProvidersContent(),
      );
      overlay.remove();
    });
    actions.appendChild(cancelBtn);
    actions.appendChild(saveBtn);
    modal.appendChild(actions);

    overlay.appendChild(modal);
    overlay.addEventListener("click", (e) => {
      if (e.target === overlay) overlay.remove();
    });
    document.body.appendChild(overlay);
  }

  // ========== TYDE + MCP TABS ==========

  private setMcpHttpServerEnabled(enabled: boolean): void {
    this.mcpHttpStatusLoading = true;
    this.mcpHttpStatusError = null;
    setMcpHttpServerEnabledBridge(enabled)
      .then((settings) => {
        this.mcpHttpServerSettings = settings;
        this.onMcpHttpSettingsChange?.(settings);
      })
      .catch((err) => {
        this.mcpHttpStatusError =
          err instanceof Error ? err.message : String(err);
        console.error("Failed to update MCP HTTP server setting:", err);
      })
      .finally(() => {
        this.mcpHttpStatusLoading = false;
        this.rerenderPanelContent("tyde", () => this.buildTydeContent());
      });
  }

  private setDriverMcpHttpServerEnabled(enabled: boolean): void {
    this.driverMcpHttpStatusLoading = true;
    this.driverMcpHttpStatusError = null;
    setDriverMcpHttpServerEnabledBridge(enabled)
      .then((settings) => {
        this.driverMcpHttpServerSettings = settings;
      })
      .catch((err) => {
        this.driverMcpHttpStatusError =
          err instanceof Error ? err.message : String(err);
        console.error("Failed to update driver MCP HTTP server setting:", err);
      })
      .finally(() => {
        this.driverMcpHttpStatusLoading = false;
        this.rerenderPanelContent("tyde", () => this.buildTydeContent());
      });
  }

  private setDriverMcpHttpServerAutoloadEnabled(enabled: boolean): void {
    this.driverMcpHttpStatusLoading = true;
    this.driverMcpHttpStatusError = null;
    setDriverMcpHttpServerAutoloadEnabledBridge(enabled)
      .then((settings) => {
        this.driverMcpHttpServerSettings = settings;
      })
      .catch((err) => {
        this.driverMcpHttpStatusError =
          err instanceof Error ? err.message : String(err);
        console.error(
          "Failed to update driver MCP HTTP autoload setting:",
          err,
        );
      })
      .finally(() => {
        this.driverMcpHttpStatusLoading = false;
        this.rerenderPanelContent("tyde", () => this.buildTydeContent());
      });
  }

  private buildMcpRuntimeControl(): HTMLElement {
    const section = el("div", { class: "settings-section" });
    section.appendChild(
      el("h3", { class: "settings-section-header" }, "Tyde MCP Control Server"),
    );

    const field = el("div", { class: "settings-field" });
    const row = el("div", { class: "settings-toggle-row" });
    const labelCol = el("div", { class: "settings-toggle-label-col" });
    labelCol.appendChild(
      el(
        "label",
        { class: "settings-label", "data-testid": "settings-label" },
        "Enable Loopback MCP Control",
      ),
    );
    labelCol.appendChild(
      el(
        "p",
        { class: "settings-description" },
        "When enabled, local MCP clients can control Tyde agents across your workspaces.",
      ),
    );

    const statusText = this.mcpHttpStatusLoading
      ? "Updating..."
      : this.mcpHttpServerSettings.running
        ? this.mcpHttpServerSettings.url
          ? `Running at ${this.mcpHttpServerSettings.url}`
          : "Running"
        : "Disabled";
    labelCol.appendChild(
      el("p", { class: "settings-description" }, statusText),
    );
    if (this.mcpHttpStatusError) {
      labelCol.appendChild(
        el(
          "p",
          { class: "settings-description" },
          `Error: ${this.mcpHttpStatusError}`,
        ),
      );
    }
    row.appendChild(labelCol);

    const toggle = el("label", { class: "settings-toggle" });
    const input = el("input", {
      type: "checkbox",
      "data-testid": "settings-mcp-http-enabled",
    }) as HTMLInputElement;
    input.checked = this.mcpHttpServerSettings.enabled;
    input.disabled = this.mcpHttpStatusLoading;
    input.addEventListener("change", () => {
      this.setMcpHttpServerEnabled(input.checked);
    });
    toggle.appendChild(input);
    toggle.appendChild(el("span", { class: "settings-toggle-slider" }));
    row.appendChild(toggle);

    field.appendChild(row);
    section.appendChild(field);
    return section;
  }

  private buildDriverMcpRuntimeControl(): HTMLElement {
    const section = el("div", { class: "settings-section" });
    section.appendChild(
      el("h3", { class: "settings-section-header" }, "Tyde MCP Driver Server"),
    );

    const field = el("div", { class: "settings-field" });
    const row = el("div", { class: "settings-toggle-row" });
    const labelCol = el("div", { class: "settings-toggle-label-col" });
    labelCol.appendChild(
      el(
        "label",
        { class: "settings-label", "data-testid": "settings-label" },
        "Enable MCP Driver",
      ),
    );
    labelCol.appendChild(
      el(
        "p",
        { class: "settings-description" },
        "When enabled, agents can spawn dev instances and remotely debug them via MCP.",
      ),
    );

    const statusText = this.driverMcpHttpStatusLoading
      ? "Updating..."
      : this.driverMcpHttpServerSettings.running
        ? this.driverMcpHttpServerSettings.url
          ? `Running at ${this.driverMcpHttpServerSettings.url}`
          : "Running"
        : "Disabled";
    labelCol.appendChild(
      el("p", { class: "settings-description" }, statusText),
    );
    if (this.driverMcpHttpStatusError) {
      labelCol.appendChild(
        el(
          "p",
          { class: "settings-description" },
          `Error: ${this.driverMcpHttpStatusError}`,
        ),
      );
    }
    row.appendChild(labelCol);

    const toggle = el("label", { class: "settings-toggle" });
    const input = el("input", {
      type: "checkbox",
      "data-testid": "settings-driver-mcp-http-enabled",
    }) as HTMLInputElement;
    input.checked = this.driverMcpHttpServerSettings.enabled;
    input.disabled = this.driverMcpHttpStatusLoading;
    input.addEventListener("change", () => {
      this.setDriverMcpHttpServerEnabled(input.checked);
    });
    toggle.appendChild(input);
    toggle.appendChild(el("span", { class: "settings-toggle-slider" }));
    row.appendChild(toggle);

    field.appendChild(row);

    const autoRow = el("div", { class: "settings-toggle-row" });
    const autoLabelCol = el("div", { class: "settings-toggle-label-col" });
    autoLabelCol.appendChild(
      el(
        "label",
        { class: "settings-label", "data-testid": "settings-label" },
        "Auto-load into new sessions",
      ),
    );
    autoLabelCol.appendChild(
      el(
        "p",
        { class: "settings-description" },
        "When enabled, new chat sessions start with the driver MCP server preconfigured.",
      ),
    );
    autoRow.appendChild(autoLabelCol);

    const autoToggle = el("label", { class: "settings-toggle" });
    const autoInput = el("input", {
      type: "checkbox",
      "data-testid": "settings-driver-mcp-http-autoload",
    }) as HTMLInputElement;
    autoInput.checked = this.driverMcpHttpServerSettings.autoload;
    autoInput.disabled =
      this.driverMcpHttpStatusLoading ||
      !this.driverMcpHttpServerSettings.enabled;
    autoInput.addEventListener("change", () => {
      this.setDriverMcpHttpServerAutoloadEnabled(autoInput.checked);
    });
    autoToggle.appendChild(autoInput);
    autoToggle.appendChild(el("span", { class: "settings-toggle-slider" }));
    autoRow.appendChild(autoToggle);
    field.appendChild(autoRow);

    section.appendChild(field);
    return section;
  }

  private buildTydePanel(): HTMLElement {
    const section = el("section", {
      class: "tab-panel",
      "data-panel": "tyde",
      role: "tabpanel",
      "data-testid": "settings-tab-panel",
    });
    section.appendChild(el("h2", { class: "tab-title" }, "Agent Control"));
    section.appendChild(this.buildTydeContent());
    return section;
  }

  private buildTydeContent(): DocumentFragment {
    const frag = document.createDocumentFragment();
    frag.appendChild(this.buildMcpRuntimeControl());
    frag.appendChild(this.buildDriverMcpRuntimeControl());
    return frag;
  }

  private buildMcpPanel(): HTMLElement {
    const section = el("section", {
      class: "tab-panel",
      "data-panel": "mcp",
      role: "tabpanel",
      "data-testid": "settings-tab-panel",
    });
    section.appendChild(el("h2", { class: "tab-title" }, "MCP Servers"));
    section.appendChild(this.buildMcpContent());
    return section;
  }

  private buildMcpContent(): DocumentFragment {
    const frag = document.createDocumentFragment();
    const list = el("div", { class: "settings-card-list" });
    for (const entry of this.getMcpEntries()) {
      list.appendChild(this.buildMcpCard(entry));
    }

    const addBtn = el(
      "button",
      { class: "settings-add-btn settings-add-btn-primary" },
      "+ Add Server",
    );
    addBtn.addEventListener("click", () => this.showMcpModal(null));

    frag.appendChild(list);
    frag.appendChild(addBtn);
    return frag;
  }

  private getMcpEntries(): McpEntry[] {
    const raw = this.backendSettings.mcp_servers;
    if (Array.isArray(raw)) {
      return raw
        .map((config, index): McpEntry | null => {
          if (!config || typeof config !== "object") return null;
          const explicitName =
            typeof config.name === "string" ? config.name.trim() : "";
          const fallbackName = `Server ${index + 1}`;
          return {
            name: explicitName || fallbackName,
            config: config as Record<string, any>,
            index,
            key: null,
          };
        })
        .filter((entry): entry is McpEntry => entry !== null);
    }
    if (raw && typeof raw === "object") {
      return Object.entries(raw as Record<string, any>).map(
        ([name, config]) => ({
          name,
          config: (config && typeof config === "object"
            ? config
            : {}) as Record<string, any>,
          index: null,
          key: name,
        }),
      );
    }
    return [];
  }

  private ensureMcpServerMap(): Record<string, any> {
    const raw = this.backendSettings.mcp_servers;
    if (raw && typeof raw === "object" && !Array.isArray(raw)) {
      return raw as Record<string, any>;
    }

    const converted: Record<string, any> = {};
    if (Array.isArray(raw)) {
      for (let i = 0; i < raw.length; i++) {
        const item = raw[i];
        if (!item || typeof item !== "object") continue;
        const explicitName =
          typeof item.name === "string" ? item.name.trim() : "";
        const fallbackName = `Server ${i + 1}`;
        const name = explicitName || fallbackName;
        const { name: _name, ...rest } = item as Record<string, any>;
        converted[name] = rest;
      }
    }
    this.backendSettings.mcp_servers = converted;
    return converted;
  }

  private upsertMcpEntry(
    existing: McpEntry | null,
    nextName: string,
    nextConfig: Record<string, any>,
  ): void {
    const servers = this.ensureMcpServerMap();
    const oldKey = existing?.key ?? existing?.name;
    if (oldKey && oldKey !== nextName) delete servers[oldKey];
    servers[nextName] = nextConfig;
  }

  private deleteMcpEntry(entry: McpEntry): void {
    const servers = this.ensureMcpServerMap();
    const key = entry.key ?? entry.name;
    delete servers[key];
  }

  private buildMcpCard(entry: McpEntry): HTMLElement {
    const { name, config: server } = entry;
    const card = el("div", { class: "settings-provider-card" });

    const info = el("div", { class: "settings-provider-info" });
    const header = el("div", { class: "settings-provider-header" });
    header.appendChild(
      el(
        "span",
        {
          class: "settings-provider-name",
          "data-testid": "settings-card-name",
        },
        name,
      ),
    );
    header.appendChild(
      el(
        "span",
        { class: "settings-status-chip status-connected" },
        "Configured",
      ),
    );
    info.appendChild(header);

    const envCount =
      server.env && typeof server.env === "object"
        ? Object.keys(server.env).length
        : 0;
    const detail = [
      server.command ? `Cmd: ${server.command}` : "",
      server.args?.length ? `Args: ${server.args.length}` : "",
      envCount > 0 ? `Env: ${envCount}` : "",
    ]
      .filter(Boolean)
      .join(" · ");
    if (detail)
      info.appendChild(
        el("div", { class: "settings-provider-detail" }, detail),
      );
    card.appendChild(info);

    const actions = el("div", { class: "settings-provider-actions" });
    const editBtn = el(
      "button",
      { class: "settings-action-btn", title: "Edit server" },
      "Edit",
    );
    editBtn.addEventListener("click", () => this.showMcpModal(entry));
    const delBtn = el(
      "button",
      {
        class: "settings-action-btn settings-provider-delete",
        title: "Delete server",
      },
      "Delete",
    );
    delBtn.addEventListener("click", () => {
      this.deleteMcpEntry(entry);
      this.persistToBackend();
      this.rerenderPanelContent("mcp", () => this.buildMcpContent());
    });
    actions.appendChild(editBtn);
    actions.appendChild(delBtn);
    card.appendChild(actions);

    return card;
  }

  private showMcpModal(existing: McpEntry | null): void {
    const isEdit = existing !== null;
    const cfg = existing?.config ?? {};
    const currentName = existing?.name ?? "";
    const fields: [string, string, string][] = [
      ["name", "Name", currentName],
      ["command", "Command", cfg.command ?? ""],
      ["args", "Arguments (one per line)", (cfg.args ?? []).join("\n")],
      [
        "env",
        "Env Vars (KEY=VALUE per line)",
        cfg.env
          ? Object.entries(cfg.env)
              .map(([k, v]) => `${k}=${v}`)
              .join("\n")
          : "",
      ],
    ];
    this.showGenericModal(
      isEdit ? "Edit MCP Server" : "Add MCP Server",
      fields,
      (values) => {
        const name = values.name.trim();
        if (!name) return;
        const obj: Record<string, any> = {
          command: values.command.trim(),
        };
        if (values.args.trim()) {
          obj.args = values.args
            .split("\n")
            .map((l: string) => l.trim())
            .filter(Boolean);
        }
        if (values.env.trim()) {
          obj.env = {};
          for (const line of values.env.split("\n")) {
            const eq = line.indexOf("=");
            if (eq === -1) continue;
            obj.env[line.substring(0, eq).trim()] = line
              .substring(eq + 1)
              .trim();
          }
        }
        this.upsertMcpEntry(existing, name, obj);
        this.persistToBackend();
        this.rerenderPanelContent("mcp", () => this.buildMcpContent());
      },
    );
  }

  // ========== AGENT MODELS TAB ==========

  private buildAgentModelsPanel(): HTMLElement {
    const section = el("section", {
      class: "tab-panel",
      "data-panel": "agent-models",
      role: "tabpanel",
      "data-testid": "settings-tab-panel",
    });
    section.appendChild(el("h2", { class: "tab-title" }, "Agent Models"));
    section.appendChild(this.buildAgentModelsContent());
    return section;
  }

  private buildAgentModelsContent(): DocumentFragment {
    const frag = document.createDocumentFragment();
    const list = el("div", { class: "settings-card-list" });
    const models: Record<string, any> = this.backendSettings.agent_models ?? {};

    for (const [name, config] of Object.entries(models)) {
      list.appendChild(this.buildAgentModelCard(name, config));
    }

    const addBtn = el(
      "button",
      { class: "settings-add-btn settings-add-btn-primary" },
      "+ Add Override",
    );
    addBtn.addEventListener("click", () =>
      this.showAgentModelModal(null, null),
    );

    frag.appendChild(list);
    frag.appendChild(addBtn);
    return frag;
  }

  private buildAgentModelCard(name: string, config: any): HTMLElement {
    const card = el("div", { class: "settings-provider-card" });

    const info = el("div", { class: "settings-provider-info" });
    const header = el("div", { class: "settings-provider-header" });
    header.appendChild(
      el(
        "span",
        {
          class: "settings-provider-name",
          "data-testid": "settings-card-name",
        },
        name,
      ),
    );
    header.appendChild(
      el(
        "span",
        { class: "settings-status-chip status-connected" },
        "Override",
      ),
    );
    info.appendChild(header);

    const detail = [config?.model ? `Model: ${config.model}` : ""]
      .filter(Boolean)
      .join(" · ");
    if (detail)
      info.appendChild(
        el("div", { class: "settings-provider-detail" }, detail),
      );
    card.appendChild(info);

    const actions = el("div", { class: "settings-provider-actions" });
    const editBtn = el(
      "button",
      { class: "settings-action-btn", title: "Edit override" },
      "Edit",
    );
    editBtn.addEventListener("click", () =>
      this.showAgentModelModal(name, config),
    );
    const delBtn = el(
      "button",
      {
        class: "settings-action-btn settings-provider-delete",
        title: "Delete override",
      },
      "Delete",
    );
    delBtn.addEventListener("click", () => {
      delete this.backendSettings.agent_models[name];
      this.persistToBackend();
      this.rerenderPanelContent("agent-models", () =>
        this.buildAgentModelsContent(),
      );
    });
    actions.appendChild(editBtn);
    actions.appendChild(delBtn);
    card.appendChild(actions);

    return card;
  }

  private showAgentModelModal(
    existingName: string | null,
    existingConfig: any | null,
  ): void {
    const isEdit = existingName !== null;
    const fields: [string, string, string][] = [
      ["agent", "Agent Name", existingName ?? ""],
      ["model", "Model", existingConfig?.model ?? ""],
    ];
    this.showGenericModal(
      isEdit ? "Edit Agent Model" : "Add Agent Model",
      fields,
      (values) => {
        const agentName = values.agent.trim();
        if (!agentName) return;
        if (!this.backendSettings.agent_models)
          this.backendSettings.agent_models = {};
        // Remove old key if name changed during edit
        if (isEdit && existingName && existingName !== agentName) {
          delete this.backendSettings.agent_models[existingName];
        }
        this.backendSettings.agent_models[agentName] = {
          model: values.model,
        };
        this.persistToBackend();
        this.rerenderPanelContent("agent-models", () =>
          this.buildAgentModelsContent(),
        );
      },
    );
  }

  // ========== ADVANCED TAB ==========

  private buildAdvancedPanel(): HTMLElement {
    const section = el("section", {
      class: "tab-panel",
      "data-panel": "advanced",
      role: "tabpanel",
      "data-testid": "settings-tab-panel",
    });
    section.appendChild(el("h2", { class: "tab-title" }, "Advanced"));
    section.appendChild(this.buildAdvancedContent());
    return section;
  }

  private buildAdvancedContent(): DocumentFragment {
    const frag = document.createDocumentFragment();
    const section = el("div", { class: "settings-section" });

    const toggleDefinitions: [string, string, string][] = [
      [
        "enable_type_analyzer",
        "Type Analyzer",
        "Enable analyzer tools (search_types, get_type_docs).",
      ],
      [
        "disable_streaming",
        "Disable Streaming",
        "When enabled, responses are returned as one complete message.",
      ],
      [
        "disable_custom_steering",
        "Disable Custom Steering",
        "Ignore custom steering docs from .tycode and external agent configs.",
      ],
    ];
    for (const [key, label, description] of toggleDefinitions) {
      section.appendChild(this.buildToggleField(key, label, description));
    }

    const spawnContextField: SelectFieldDef = {
      key: "spawn_context_mode",
      label: "Spawn Context Mode",
      description:
        "Choose whether spawned agents fork current context or start fresh.",
      options: ["Fork", "Fresh"],
      aliases: {
        fork: "Fork",
        fresh: "Fresh",
      },
    };
    section.appendChild(this.buildSelectField(spawnContextField));

    const outputModeField: SelectFieldDef = {
      key: "run_build_test_output_mode",
      label: "Run/Build/Test Output Mode",
      description:
        "Choose whether command output appears in tool response or context.",
      options: ["ToolResponse", "Context"],
      humanLabels: {
        ToolResponse: "Tool response",
        Context: "Context",
      },
      aliases: {
        tool_response: "ToolResponse",
        context: "Context",
      },
    };
    section.appendChild(this.buildSelectField(outputModeField));

    frag.appendChild(section);
    return frag;
  }

  private buildToggleField(
    key: string,
    label: string,
    description?: string,
  ): HTMLElement {
    const field = el("div", { class: "settings-field" });
    const row = el("div", { class: "settings-toggle-row" });
    const labelCol = el("div", { class: "settings-toggle-label-col" });
    labelCol.appendChild(
      el(
        "label",
        { class: "settings-label", "data-testid": "settings-label" },
        label,
      ),
    );
    if (description) {
      labelCol.appendChild(
        el("p", { class: "settings-description" }, description),
      );
    }
    row.appendChild(labelCol);

    const toggle = el("label", { class: "settings-toggle" });
    const input = el("input", { type: "checkbox" }) as HTMLInputElement;
    if (this.backendSettings[key]) input.checked = true;
    input.addEventListener("change", () => {
      this.backendSettings[key] = input.checked;
      this.persistToBackend();
    });
    toggle.appendChild(input);
    toggle.appendChild(el("span", { class: "settings-toggle-slider" }));
    row.appendChild(toggle);
    field.appendChild(row);
    return field;
  }

  // ========== DYNAMIC MODULE TABS ==========

  private renderModuleTabs(): void {
    // Remove existing module nav buttons and panels
    for (const existing of this.container.querySelectorAll(
      ".nav-item[data-module]",
    ))
      existing.remove();
    for (const existing of this.container.querySelectorAll(
      ".tab-panel[data-module]",
    ))
      existing.remove();

    const nav =
      this.container.querySelector(
        '.settings-nav-group[data-group="ai"] .settings-nav-group-items',
      ) ?? this.container.querySelector(".settings-nav");
    const content = this.container.querySelector(".settings-content");
    if (!nav || !content) return;

    for (const moduleInfo of this.moduleSchemas) {
      const namespace = moduleInfo.namespace;
      const schema = moduleInfo.schema as any;
      if (!schema?.properties) continue;

      // Nav button
      const tabId = `module-${namespace}`;
      const navBtn = el(
        "button",
        {
          class: "nav-item",
          "data-tab": tabId,
          "data-module": namespace,
          role: "tab",
          "data-testid": "settings-nav-item",
        },
        schema.title ?? namespace.charAt(0).toUpperCase() + namespace.slice(1),
      );
      navBtn.addEventListener("click", () => this.switchTab(tabId));
      nav.appendChild(navBtn);

      // Tab panel
      const panel = el("section", {
        class: "tab-panel",
        "data-panel": tabId,
        "data-module": namespace,
        role: "tabpanel",
        "data-testid": "settings-tab-panel",
      });
      panel.appendChild(
        el(
          "h2",
          { class: "tab-title" },
          `${schema.title ?? namespace} Settings`,
        ),
      );

      if (schema.description) {
        panel.appendChild(
          el("p", { class: "settings-description" }, schema.description),
        );
      }

      const sec = el("div", { class: "settings-section" });
      const moduleSettings = this.backendSettings.modules?.[namespace] ?? {};

      for (const [fieldName, fieldSchema] of Object.entries(
        schema.properties as Record<string, any>,
      )) {
        sec.appendChild(
          this.renderSchemaField(
            namespace,
            fieldName,
            fieldSchema,
            moduleSettings[fieldName],
            schema,
          ),
        );
      }
      panel.appendChild(sec);
      content.appendChild(panel);
    }

    // Re-sync tab visibility in case active tab was a module tab
    this.syncTabVisibility();
    if (this.searchQuery.length > 0) {
      this.filterSettings(this.searchQuery);
    }
  }

  private renderSchemaField(
    namespace: string,
    fieldName: string,
    fieldSchema: any,
    currentValue: any,
    rootSchema: any,
  ): HTMLElement {
    const resolved = resolveSchemaRef(fieldSchema, rootSchema);
    const label = fieldName
      .replace(/_/g, " ")
      .replace(/\b\w/g, (c) => c.toUpperCase());
    const effectiveValue =
      currentValue !== undefined ? currentValue : resolved?.default;

    const field = el("div", { class: "settings-field" });
    field.appendChild(
      el(
        "label",
        { class: "settings-label", "data-testid": "settings-label" },
        label,
      ),
    );
    if (resolved?.description) {
      field.appendChild(
        el("p", { class: "settings-description" }, resolved.description),
      );
    }

    const onChange = (value: any) => {
      if (!this.backendSettings.modules) this.backendSettings.modules = {};
      if (!this.backendSettings.modules[namespace])
        this.backendSettings.modules[namespace] = {};
      if (value === undefined) {
        delete this.backendSettings.modules[namespace][fieldName];
      } else {
        this.backendSettings.modules[namespace][fieldName] = value;
      }
      this.persistToBackend();
    };

    const inputEl = this.createSchemaInput(
      resolved,
      rootSchema,
      effectiveValue,
      onChange,
    );
    field.appendChild(inputEl);
    return field;
  }

  private createSchemaInput(
    resolved: any,
    rootSchema: any,
    effectiveValue: any,
    onChange: (v: any) => void,
  ): HTMLElement {
    // Boolean → select Enabled/Disabled
    if (resolved?.type === "boolean") {
      return this.buildBooleanSelect(effectiveValue === true, onChange);
    }

    // Enum array → select
    if (Array.isArray(resolved?.enum)) {
      return this.buildEnumSelect(resolved.enum, effectiveValue, onChange);
    }

    // Nullable number (schemars pattern)
    if (isNullableNumber(resolved, rootSchema)) {
      return this.buildNumberInput(effectiveValue, onChange);
    }

    // oneOf with const values → select
    if (Array.isArray(resolved?.oneOf)) {
      const enumValues = this.extractOneOfValues(resolved.oneOf);
      if (enumValues.length > 0) {
        return this.buildEnumSelect(enumValues, effectiveValue, onChange);
      }
    }

    // Plain number/integer
    if (resolved?.type === "number" || resolved?.type === "integer") {
      return this.buildNumberInput(effectiveValue, onChange);
    }

    // Fallback: text input
    return this.buildTextInput(effectiveValue, onChange);
  }

  private extractOneOfValues(oneOf: any[]): any[] {
    const values: any[] = [];
    for (const opt of oneOf) {
      if (opt.const !== undefined) {
        values.push(opt.const);
        continue;
      }
      if (opt.enum?.length === 1) values.push(opt.enum[0]);
    }
    return values;
  }

  private buildBooleanSelect(
    currentTrue: boolean,
    onChange: (v: any) => void,
  ): HTMLElement {
    const select = el("select", {
      class: "settings-select",
      "data-testid": "settings-select",
    });
    const off = el("option", { value: "false" }, "Disabled");
    const on = el("option", { value: "true" }, "Enabled");
    if (!currentTrue) off.selected = true;
    else on.selected = true;
    select.appendChild(off);
    select.appendChild(on);
    select.addEventListener("change", () => onChange(select.value === "true"));
    return select;
  }

  private buildEnumSelect(
    values: any[],
    current: any,
    onChange: (v: any) => void,
  ): HTMLElement {
    const select = el("select", {
      class: "settings-select",
      "data-testid": "settings-select",
    });
    for (const v of values) {
      const opt = el("option", { value: String(v) }, String(v));
      if (v === current) opt.selected = true;
      select.appendChild(opt);
    }
    select.addEventListener("change", () => onChange(select.value));
    return select;
  }

  private buildNumberInput(
    current: any,
    onChange: (v: any) => void,
  ): HTMLElement {
    const input = el("input", { class: "settings-input", type: "number" });
    if (current !== undefined && current !== null)
      (input as HTMLInputElement).value = String(current);
    input.addEventListener("change", () => {
      const v = parseFloat((input as HTMLInputElement).value);
      onChange(Number.isNaN(v) ? undefined : v);
    });
    return input;
  }

  private buildTextInput(
    current: any,
    onChange: (v: any) => void,
  ): HTMLElement {
    const input = el("input", { class: "settings-input", type: "text" });
    if (current !== undefined && current !== null)
      (input as HTMLInputElement).value = String(current);
    input.addEventListener("change", () =>
      onChange((input as HTMLInputElement).value),
    );
    return input;
  }

  // ========== PROFILES ==========

  private buildDefaultBackendSection(): HTMLElement {
    const wrapper = el("div", { class: "settings-profile-section" });
    wrapper.appendChild(
      el(
        "label",
        { class: "settings-label", "data-testid": "settings-label" },
        "Default Backend",
      ),
    );
    const row = el("div", { class: "settings-profile-row" });
    const backendSelect = el("select", {
      class: "settings-select settings-profile-select",
      "aria-label": "Default backend",
      "data-testid": "default-backend-select",
    });
    backendSelect.addEventListener("change", () => {
      this.defaultBackend = normalizeBackendKind(backendSelect.value);
      setDefaultBackend(this.defaultBackend);
    });
    row.appendChild(backendSelect);
    wrapper.appendChild(row);
    return wrapper;
  }

  private buildProfileSection(): HTMLElement {
    const wrapper = el("div", { class: "settings-profile-section" });
    wrapper.appendChild(
      el(
        "label",
        { class: "settings-label", "data-testid": "settings-label" },
        "Tycode Profile",
      ),
    );

    const row = el("div", { class: "settings-profile-row" });
    const select = el("select", {
      class: "settings-select settings-profile-select",
      "aria-label": "Settings profile",
      "data-testid": "profile-select",
    });
    select.addEventListener("change", () => {
      if (this.adminId === null) return;
      adminSwitchProfile(this.adminId, select.value);
    });
    row.appendChild(select);

    const refreshBtn = el(
      "button",
      { class: "settings-refresh-btn", title: "Refresh profiles" },
      "↻",
    );
    refreshBtn.addEventListener("click", () => {
      if (this.adminId === null) return;
      adminListProfiles(this.adminId);
    });
    row.appendChild(refreshBtn);

    wrapper.appendChild(row);

    wrapper.appendChild(
      el(
        "label",
        { class: "settings-label", "data-testid": "settings-label" },
        "Default Tycode Profile For New Agents",
      ),
    );
    const spawnRow = el("div", { class: "settings-profile-row" });
    const spawnSelect = el("select", {
      class: "settings-select settings-profile-select",
      "aria-label": "Default Tycode profile for new agents",
      "data-testid": "spawn-profile-select",
    });
    spawnSelect.addEventListener("change", () => {
      const selected = normalizeProfileName(spawnSelect.value);
      this.defaultSpawnProfile = selected;
      setDefaultSpawnProfile(selected);
    });
    spawnRow.appendChild(spawnSelect);
    wrapper.appendChild(spawnRow);

    return wrapper;
  }

  private syncProfileDropdown(): void {
    const profileSelect = this.container.querySelector(
      '[data-testid="profile-select"]',
    ) as HTMLSelectElement | null;
    const spawnSelect = this.container.querySelector(
      '[data-testid="spawn-profile-select"]',
    ) as HTMLSelectElement | null;
    const backendSelect = this.container.querySelector(
      '[data-testid="default-backend-select"]',
    ) as HTMLSelectElement | null;
    if (!profileSelect && !spawnSelect && !backendSelect) return;

    if (backendSelect) {
      backendSelect.innerHTML = "";
      const enabledBackends = getEnabledBackends();
      const backendLabels: Record<BackendKind, string> = {
        tycode: "Tycode",
        codex: "Codex",
        claude: "Claude Code",
        kiro: "Kiro",
      };
      for (const kind of enabledBackends) {
        const opt = el("option", { value: kind }, backendLabels[kind]);
        if (kind === this.defaultBackend) opt.selected = true;
        backendSelect.appendChild(opt);
      }
      if (
        enabledBackends.length > 0 &&
        !enabledBackends.includes(this.defaultBackend)
      ) {
        this.defaultBackend = enabledBackends[0];
        setDefaultBackend(this.defaultBackend);
      }
    }

    if (profileSelect) {
      profileSelect.innerHTML = "";
      if (this.profiles.length === 0) {
        const opt = el(
          "option",
          { value: "", disabled: "true" },
          this.activeProfile ?? "No profiles loaded",
        );
        opt.selected = true;
        profileSelect.appendChild(opt);
      } else {
        const effectiveProfile = this.activeProfile ?? this.profiles[0];
        for (const profile of this.profiles) {
          const opt = el("option", { value: profile }, profile);
          if (profile === effectiveProfile) opt.selected = true;
          profileSelect.appendChild(opt);
        }
      }
    }

    if (!spawnSelect) return;
    spawnSelect.innerHTML = "";
    if (this.profiles.length === 0) {
      const opt = el(
        "option",
        { value: "", disabled: "true" },
        "No profiles loaded",
      );
      opt.selected = true;
      spawnSelect.appendChild(opt);
      return;
    }

    if (
      this.defaultSpawnProfile !== null &&
      !this.profiles.includes(this.defaultSpawnProfile)
    ) {
      this.defaultSpawnProfile = null;
      setDefaultSpawnProfile(null);
    }

    const noOverride = el("option", { value: "" }, "No override");
    if (this.defaultSpawnProfile === null) noOverride.selected = true;
    spawnSelect.appendChild(noOverride);

    for (const profile of this.profiles) {
      const opt = el("option", { value: profile }, profile);
      if (profile === this.defaultSpawnProfile) opt.selected = true;
      spawnSelect.appendChild(opt);
    }
  }

  // ========== GENERIC MODAL ==========

  private showGenericModal(
    title: string,
    fields: [string, string, string][],
    onSave: (values: Record<string, string>) => void,
  ): void {
    const overlay = el("div", { class: "settings-modal-overlay" });
    const modal = el("div", { class: "settings-modal" });
    modal.appendChild(el("h3", {}, title));

    const inputs: Record<string, HTMLInputElement | HTMLTextAreaElement> = {};
    for (const [key, label, value] of fields) {
      const fieldEl = el("div", { class: "settings-field" });
      fieldEl.appendChild(
        el(
          "label",
          { class: "settings-label", "data-testid": "settings-label" },
          label,
        ),
      );

      // Use textarea for multiline-hint fields
      const isMultiline =
        key === "args" || key === "env" || key === "custom_headers";
      if (isMultiline) {
        const ta = el("textarea", { class: "settings-textarea", rows: "3" });
        ta.value = value;
        inputs[key] = ta;
        fieldEl.appendChild(ta);
      } else {
        const inp = el("input", {
          class: "settings-input",
          type: "text",
          value,
        });
        inputs[key] = inp;
        fieldEl.appendChild(inp);
      }
      modal.appendChild(fieldEl);
    }

    const actions = el("div", { class: "settings-modal-actions" });
    const cancelBtn = el("button", {}, "Cancel");
    cancelBtn.addEventListener("click", () => overlay.remove());
    const saveBtn = el("button", { class: "settings-modal-save" }, "Save");
    saveBtn.addEventListener("click", () => {
      const values: Record<string, string> = {};
      for (const [key, input] of Object.entries(inputs))
        values[key] = input.value;
      overlay.remove();
      onSave(values);
    });
    actions.appendChild(cancelBtn);
    actions.appendChild(saveBtn);
    modal.appendChild(actions);

    overlay.appendChild(modal);
    // Close on overlay background click
    overlay.addEventListener("click", (e) => {
      if (e.target === overlay) overlay.remove();
    });
    document.body.appendChild(overlay);
  }

  // ========== SHARED HELPERS ==========

  private setActiveSegment(control: HTMLElement, value: string): void {
    for (const btn of control.querySelectorAll(".segment")) {
      btn.classList.remove("active");
      btn.setAttribute("aria-checked", "false");
    }
    const target = control.querySelector(
      `.segment[data-value="${value}"]`,
    ) as HTMLElement | null;
    if (!target) return;
    target.classList.add("active");
    target.setAttribute("aria-checked", "true");
  }
}
