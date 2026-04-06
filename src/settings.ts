import { invoke } from "@tauri-apps/api/core";
import type { AgentDefinitionStore } from "./agent_defs/store";
import type { AgentDefinitionEntry, AgentMcpServer } from "./agent_defs/types";
import {
  addHost as addHostBridge,
  adminGetModuleSchemas,
  adminGetSettings,
  adminListProfiles,
  adminSwitchProfile,
  adminUpdateSettings,
  type BackendDependencyStatus,
  type BackendDepResult,
  type BackendKind,
  type BackendUsageResult,
  type BackendUsageWindow,
  checkBackendDependencies as checkBackendDependenciesBridge,
  type DriverMcpHttpServerSettings,
  getDriverMcpHttpServerSettings as getDriverMcpHttpServerSettingsBridge,
  getMcpHttpServerSettings as getMcpHttpServerSettingsBridge,
  getRemoteControlSettings as getRemoteControlSettingsBridge,
  getRemoteTydeServerStatus as getRemoteTydeServerStatusBridge,
  type Host,
  installAndLaunchRemoteTydeServer as installAndLaunchRemoteTydeServerBridge,
  installBackendDependency as installBackendDependencyBridge,
  installRemoteTydeServer as installRemoteTydeServerBridge,
  launchRemoteTydeServer as launchRemoteTydeServerBridge,
  listHosts as listHostsBridge,
  type McpHttpServerSettings,
  normalizeBackendKind,
  queryBackendUsage as queryBackendUsageBridge,
  type RemoteTydeServerStatus,
  removeHost as removeHostBridge,
  setDriverMcpHttpServerAutoloadEnabled as setDriverMcpHttpServerAutoloadEnabledBridge,
  setDriverMcpHttpServerEnabled as setDriverMcpHttpServerEnabledBridge,
  setMcpHttpServerEnabled as setMcpHttpServerEnabledBridge,
  setRemoteControlEnabled as setRemoteControlEnabledBridge,
  updateHostDefaultBackend as updateHostDefaultBackendsBridge,
  updateHostEnabledBackends as updateHostEnabledBackendsBridge,
  upgradeRemoteTydeServer as upgradeRemoteTydeServerBridge,
} from "./bridge";
import {
  broadcastToolOutputMode,
  getToolOutputMode,
  onToolOutputModeChange,
  setToolOutputMode,
  type ToolOutputMode,
} from "./chat/tools";
import {
  type DiffContextMode,
  type DiffViewMode,
  getDiffSettings,
  onDiffSettingsChange,
  setDiffSettings,
} from "./diff_settings";
import type { NotificationManager } from "./notifications";

const APPEARANCE_STORAGE_KEY = "tyde-appearance";
const ACTIVE_SETTINGS_TAB_KEY = "tyde-settings-active-tab";
const ONBOARDING_COMPLETE_KEY = "tyde-onboarding-complete";
const DEFAULT_SPAWN_PROFILE_STORAGE_KEY = "tyde-default-spawn-profile";
const SELECTED_HOST_STORAGE_KEY = "tyde-selected-settings-host";

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
const VALID_SETTINGS_TABS = new Set<SettingsTabId>([
  "appearance",
  "notifications",
  "backends",
  "agents",
  "tyde",
  "general",
  "providers",
  "mcp",
  "agent-models",
  "advanced",
]);

function loadActiveTab(): SettingsTabId {
  const stored = localStorage.getItem(ACTIVE_SETTINGS_TAB_KEY) ?? "appearance";
  return VALID_SETTINGS_TABS.has(stored) ? stored : "appearance";
}

function saveActiveTab(tab: SettingsTabId): void {
  localStorage.setItem(ACTIVE_SETTINGS_TAB_KEY, tab);
}

function loadSelectedHostId(): string | null {
  return localStorage.getItem(SELECTED_HOST_STORAGE_KEY);
}

function saveSelectedHostId(hostId: string): void {
  localStorage.setItem(SELECTED_HOST_STORAGE_KEY, hostId);
}

function normalizeProfileName(value: string | null): string | null {
  if (value === null) return null;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
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

// --- Backend dependencies ---

const ALL_BACKENDS: BackendKind[] = [
  "tycode",
  "codex",
  "claude",
  "kiro",
  "gemini",
];
type UsageAwareBackendKind = Exclude<BackendKind, "tycode" | "claude">;
const USAGE_AWARE_BACKENDS: UsageAwareBackendKind[] = ["codex", "kiro"];

let cachedDependencyStatus: Record<BackendKind, BackendDepResult> | null = null;

export function setCachedDependencyStatus(
  status: BackendDependencyStatus,
): void {
  cachedDependencyStatus = {
    tycode: status.tycode,
    codex: status.codex,
    claude: status.claude,
    kiro: status.kiro,
    gemini: status.gemini,
  };
}

export function getCachedDependencyStatus(): Record<
  BackendKind,
  BackendDepResult
> | null {
  return cachedDependencyStatus;
}

export async function initializeBackendDependencies(): Promise<void> {
  try {
    const status = await checkBackendDependenciesBridge();
    setCachedDependencyStatus(status);
  } catch (err) {
    console.error("Failed to initialize backend dependencies:", err);
  }
}

/**
 * Returns true if tycode was previously enabled by the user but the binary
 * is no longer available (version bump after app update).
 */
export function needsTycodeUpgrade(): boolean {
  if (!isOnboardingComplete()) return false;
  const status = getCachedDependencyStatus();
  return status !== null && !status.tycode.available;
}

// --- Onboarding ---

export function isOnboardingComplete(): boolean {
  return localStorage.getItem(ONBOARDING_COMPLETE_KEY) !== null;
}

export function markOnboardingComplete(): void {
  localStorage.setItem(ONBOARDING_COMPLETE_KEY, "true");
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
  onHostChange: ((host: Host | null) => void) | null = null;
  onHostsUpdated: (() => void) | null = null;
  onSpawnAgent:
    | ((
        definitionId: string,
        backendOverride: BackendKind | undefined,
        host: Host | null,
      ) => void)
    | null = null;

  private container: HTMLElement;
  private appearance: AppearanceSettings;
  private backendSettings: Record<string, any> = {};
  private moduleSchemas: any[] = [];
  private profiles: string[] = [];
  private activeProfile: string | null = null;
  private defaultSpawnProfile: string | null = getDefaultSpawnProfile();
  private activeTab: SettingsTabId;
  private searchQuery = "";
  private backendDependencyStatus: Record<
    BackendKind,
    BackendDepResult
  > | null = null;
  private installingBackends: Set<BackendKind> = new Set();
  private backendInstallError: Map<BackendKind, string> = new Map();
  private backendUsage: Partial<
    Record<UsageAwareBackendKind, BackendUsageResult>
  > = {};
  private backendUsageLoading: Set<UsageAwareBackendKind> = new Set();
  private backendUsageError: Map<UsageAwareBackendKind, string> = new Map();
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
  private remoteControlSettings: {
    enabled: boolean;
    running: boolean;
    socket_path: string | null;
    connected_clients: number;
  } = {
    enabled: false,
    running: false,
    socket_path: null,
    connected_clients: 0,
  };
  private remoteControlLoading = false;
  private remoteTydeServerStatus: RemoteTydeServerStatus | null = null;
  private remoteTydeServerStatusLoading = false;
  private remoteTydeServerStatusError: string | null = null;
  private remoteTydeServerStatusSeq = 0;
  private remoteTydeServerActionLoading:
    | "install"
    | "launch"
    | "install_launch"
    | "upgrade"
    | null = null;
  private hosts: Host[] = [];
  private hostsLoading = false;
  private selectedHostId: string | null = loadSelectedHostId();
  private agentDefinitionStore: AgentDefinitionStore | null = null;
  private agentDefinitionUnavailableReason: string | null = null;
  private notificationManager: NotificationManager | null = null;
  private _adminId: number | null = null;
  private unsubDiffSettings: (() => void) | null = null;

  get adminId(): number | null {
    return this._adminId;
  }

  set adminId(id: number | null) {
    if (id === this._adminId) return;
    this._adminId = id;
    this.backendSettings = {};
    this.moduleSchemas = [];
    this.profiles = [];
    this.activeProfile = null;
    this.rerenderBackendTabs();
    this.renderModuleTabs();
    this.syncProfileDropdown();
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
    this.refreshRemoteControlSettings();
    this.refreshBackendDependencies();
    this.render();
  }

  setAgentDefinitionStore(
    store: AgentDefinitionStore | null,
    unavailableReason: string | null = null,
  ): void {
    this.agentDefinitionStore = store;
    this.agentDefinitionUnavailableReason = unavailableReason;
    this.rerenderPanelContent("agents", () => this.buildAgentsContent());
  }

  setNotificationManager(manager: NotificationManager): void {
    this.notificationManager = manager;
    manager.onEnabledChange = (enabled) => {
      const input = this.container.querySelector(
        '[data-testid="settings-notifications-enabled"]',
      ) as HTMLInputElement | null;
      if (input) input.checked = enabled;
    };
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
        this.rerenderPanelContent("tyde", () => this.buildTydeContent());
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
        this.rerenderPanelContent("tyde", () => this.buildTydeContent());
      });
  }

  private refreshRemoteControlSettings(): void {
    this.remoteControlLoading = true;
    getRemoteControlSettingsBridge()
      .then((settings) => {
        this.remoteControlSettings = settings;
      })
      .catch((err) => {
        console.error("Failed to load remote control settings:", err);
      })
      .finally(() => {
        this.remoteControlLoading = false;
        this.rerenderPanelContent("tyde", () => this.buildTydeContent());
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
          gemini: status.gemini,
        };
        setCachedDependencyStatus(status);
        this.rerenderPanelContent("backends", () =>
          this.buildBackendsContent(),
        );
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

  private getSelectedHost(): Host | null {
    if (this.hosts.length === 0) return null;
    if (this.selectedHostId) {
      const selected = this.hosts.find((h) => h.id === this.selectedHostId);
      if (selected) return selected;
    }
    const local = this.hosts.find((h) => h.is_local);
    return local ?? this.hosts[0];
  }

  private setSelectedHost(hostId: string, notify = true): void {
    if (this.selectedHostId === hostId) return;
    this.selectedHostId = hostId;
    saveSelectedHostId(hostId);
    this.backendUsage = {};
    this.backendUsageError.clear();
    this.backendUsageLoading.clear();
    this.remoteTydeServerStatus = null;
    this.remoteTydeServerStatusError = null;
    this.rerenderHostToolbar();
    this.rerenderPanelContent("backends", () => this.buildBackendsContent());
    this.rerenderPanelContent("tyde", () => this.buildTydeContent());
    this.refreshRemoteTydeServerStatusForSelectedHost();
    this.syncProfileDropdown();
    if (notify) this.notifySelectedHostChanged();
  }

  private ensureSelectedHost(notify = true): void {
    const selected = this.getSelectedHost();
    if (!selected) return;
    const changed = this.selectedHostId !== selected.id;
    if (changed) {
      this.selectedHostId = selected.id;
      saveSelectedHostId(selected.id);
      this.remoteTydeServerStatus = null;
      this.remoteTydeServerStatusError = null;
      this.refreshRemoteTydeServerStatusForSelectedHost();
    }
    if (notify && changed) this.notifySelectedHostChanged();
  }

  notifySelectedHostChanged(): void {
    const host = this.getSelectedHost();
    this.onHostChange?.(host);
  }

  private hostUsesRemoteTydeServer(host: Host | null): host is Host {
    return !!host && !host.is_local && host.remote_kind === "tyde_server";
  }

  private refreshRemoteTydeServerStatusForSelectedHost(): void {
    const selectedHost = this.getSelectedHost();
    if (!this.hostUsesRemoteTydeServer(selectedHost)) {
      this.remoteTydeServerStatus = null;
      this.remoteTydeServerStatusError = null;
      this.remoteTydeServerStatusLoading = false;
      this.remoteTydeServerActionLoading = null;
      this.rerenderPanelContent("tyde", () => this.buildTydeContent());
      return;
    }

    const seq = ++this.remoteTydeServerStatusSeq;
    this.remoteTydeServerStatusLoading = true;
    this.remoteTydeServerStatusError = null;
    this.rerenderPanelContent("tyde", () => this.buildTydeContent());

    getRemoteTydeServerStatusBridge(selectedHost.id)
      .then((status) => {
        if (seq !== this.remoteTydeServerStatusSeq) return;
        this.remoteTydeServerStatus = status;
      })
      .catch((err) => {
        if (seq !== this.remoteTydeServerStatusSeq) return;
        this.remoteTydeServerStatusError =
          err instanceof Error ? err.message : String(err);
      })
      .finally(() => {
        if (seq !== this.remoteTydeServerStatusSeq) return;
        this.remoteTydeServerStatusLoading = false;
        this.rerenderPanelContent("tyde", () => this.buildTydeContent());
      });
  }

  private runRemoteTydeServerAction(
    action: "install" | "launch" | "install_launch" | "upgrade",
  ): void {
    const selectedHost = this.getSelectedHost();
    if (!this.hostUsesRemoteTydeServer(selectedHost)) return;

    this.remoteTydeServerActionLoading = action;
    this.remoteTydeServerStatusError = null;
    this.rerenderPanelContent("tyde", () => this.buildTydeContent());

    const run = () => {
      switch (action) {
        case "install":
          return installRemoteTydeServerBridge(selectedHost.id);
        case "launch":
          return launchRemoteTydeServerBridge(selectedHost.id);
        case "install_launch":
          return installAndLaunchRemoteTydeServerBridge(selectedHost.id);
        case "upgrade":
          return upgradeRemoteTydeServerBridge(selectedHost.id);
      }
    };

    run()
      .then((status) => {
        this.remoteTydeServerStatus = status;
      })
      .catch((err) => {
        const detail = err instanceof Error ? err.message : String(err);
        this.remoteTydeServerStatusError = detail;
      })
      .finally(() => {
        this.remoteTydeServerActionLoading = null;
        this.refreshRemoteTydeServerStatusForSelectedHost();
      });
  }

  private buildHostToolbar(): HTMLElement {
    const toolbar = el("div", { class: "settings-host-toolbar" });
    toolbar.dataset.testid = "settings-host-toolbar";

    const titleRow = el("div", { class: "settings-host-toolbar-header" });
    const label = el(
      "label",
      {
        class: "settings-label settings-host-toolbar-label",
        "data-testid": "settings-label",
      },
      "Settings Host",
    );
    label.setAttribute("for", "settings-host-select");
    titleRow.appendChild(label);
    const selectedHost = this.getSelectedHost();
    const subtitleText = selectedHost
      ? selectedHost.is_local
        ? "Local machine"
        : selectedHost.remote_kind === "tyde_server"
          ? `${selectedHost.hostname} (Tyde Server)`
          : selectedHost.hostname
      : "No hosts configured";
    const subtitle = el(
      "p",
      { class: "settings-description settings-host-toolbar-subtitle" },
      subtitleText,
    );
    titleRow.appendChild(subtitle);
    toolbar.appendChild(titleRow);

    const row = el("div", {
      class: "settings-profile-row settings-host-toolbar-row",
    });
    const hostSelect = el("select", {
      class: "settings-select settings-profile-select settings-host-select",
      "aria-label": "Settings host",
      id: "settings-host-select",
      "data-testid": "settings-host-select",
    }) as HTMLSelectElement;
    hostSelect.disabled = this.hostsLoading || this.hosts.length === 0;
    for (const host of this.hosts) {
      const hostLabel = host.is_local
        ? host.label
        : `${host.label} • ${host.hostname}`;
      const opt = el("option", { value: host.id }, hostLabel);
      if (this.getSelectedHost()?.id === host.id) opt.selected = true;
      hostSelect.appendChild(opt);
    }
    hostSelect.addEventListener("change", () => {
      this.setSelectedHost(hostSelect.value);
    });
    row.appendChild(hostSelect);

    const addBtn = el(
      "button",
      {
        class: "settings-host-toolbar-btn",
        type: "button",
        "data-testid": "settings-host-add",
      },
      "Add Remote",
    );
    addBtn.addEventListener("click", () => this.showAddHostModal());
    row.appendChild(addBtn);

    if (selectedHost && !selectedHost.is_local) {
      const removeBtn = el(
        "button",
        {
          class: "settings-host-toolbar-btn settings-host-toolbar-btn-danger",
          type: "button",
          "data-testid": "settings-host-remove",
        },
        "Remove",
      );
      removeBtn.addEventListener("click", () => {
        removeHostBridge(selectedHost.id)
          .then(() => this.refreshHosts())
          .catch((err) => console.error("Failed to remove host:", err));
      });
      row.appendChild(removeBtn);
    }

    toolbar.appendChild(row);
    return toolbar;
  }

  private rerenderHostToolbar(): void {
    const oldToolbar = this.container.querySelector(".settings-host-toolbar");
    if (!oldToolbar) return;
    oldToolbar.replaceWith(this.buildHostToolbar());
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
    panel.appendChild(this.buildHostToolbar());

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
      this.activeTab === "notifications" ||
      this.activeTab === "backends" ||
      this.activeTab === "agents" ||
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
    const notificationsBtn = el(
      "button",
      {
        class: "nav-item",
        "data-tab": "notifications",
        role: "tab",
        "data-testid": "settings-nav-item",
      },
      "Notifications",
    );
    notificationsBtn.addEventListener("click", () =>
      this.switchTab("notifications"),
    );
    uiItems.appendChild(notificationsBtn);
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
    const agentsBtn = el(
      "button",
      {
        class: "nav-item",
        "data-tab": "agents",
        role: "tab",
        "data-testid": "settings-nav-item",
      },
      "Agents",
    );
    agentsBtn.addEventListener("click", () => this.switchTab("agents"));
    uiItems.appendChild(agentsBtn);
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
    content.appendChild(this.buildNotificationsPanel());
    content.appendChild(this.buildBackendsPanel());
    content.appendChild(this.buildAgentsPanel());
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
      this.activeTab === "agents" ||
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

  openToTab(tab: SettingsTabId): void {
    this.switchTab(tab);
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

    // Diff View
    const diffSection = el("div", { class: "settings-section" });
    diffSection.appendChild(
      el("h3", { class: "settings-section-header" }, "Diff View"),
    );

    // Diff Layout
    const layoutField = el("div", { class: "settings-field" });
    layoutField.appendChild(
      el(
        "label",
        { class: "settings-label", "data-testid": "settings-label" },
        "Diff Layout",
      ),
    );
    layoutField.appendChild(
      el(
        "p",
        { class: "settings-description" },
        "Choose between unified (inline) and side-by-side view.",
      ),
    );

    const layoutControl = el("div", {
      class: "settings-segmented-control",
      "data-setting": "diff-view-mode",
      role: "radiogroup",
    });
    for (const [v, label] of [
      ["unified", "Unified"],
      ["side-by-side", "Side-by-Side"],
    ]) {
      const btn = el(
        "button",
        {
          class: "segment",
          "data-value": v,
          role: "radio",
          "aria-checked": "false",
        },
        label,
      );
      btn.addEventListener("click", () => {
        setDiffSettings({ viewMode: v as DiffViewMode });
        this.setActiveSegment(layoutControl, v);
      });
      layoutControl.appendChild(btn);
    }
    layoutField.appendChild(layoutControl);
    diffSection.appendChild(layoutField);

    // Diff Context
    const contextField = el("div", { class: "settings-field" });
    contextField.appendChild(
      el(
        "label",
        { class: "settings-label", "data-testid": "settings-label" },
        "Diff Context",
      ),
    );
    contextField.appendChild(
      el(
        "p",
        { class: "settings-description" },
        "Prefer showing only changed hunks or full file context.",
      ),
    );

    const contextControl = el("div", {
      class: "settings-segmented-control",
      "data-setting": "diff-context-mode",
      role: "radiogroup",
    });
    for (const [v, label] of [
      ["hunks", "Hunks Only"],
      ["full", "Full Context"],
    ]) {
      const btn = el(
        "button",
        {
          class: "segment",
          "data-value": v,
          role: "radio",
          "aria-checked": "false",
        },
        label,
      );
      btn.addEventListener("click", () => {
        setDiffSettings({ contextMode: v as DiffContextMode });
        this.setActiveSegment(contextControl, v);
      });
      contextControl.appendChild(btn);
    }
    contextField.appendChild(contextControl);
    diffSection.appendChild(contextField);

    this.unsubDiffSettings?.();
    this.unsubDiffSettings = onDiffSettingsChange((settings) => {
      this.setActiveSegment(layoutControl, settings.viewMode);
      this.setActiveSegment(contextControl, settings.contextMode);
    });

    section.appendChild(diffSection);

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

    const diffSettings = getDiffSettings();
    const layoutControl = this.container.querySelector(
      '.settings-segmented-control[data-setting="diff-view-mode"]',
    ) as HTMLElement | null;
    if (layoutControl)
      this.setActiveSegment(layoutControl, diffSettings.viewMode);

    const contextControl = this.container.querySelector(
      '.settings-segmented-control[data-setting="diff-context-mode"]',
    ) as HTMLElement | null;
    if (contextControl)
      this.setActiveSegment(contextControl, diffSettings.contextMode);
  }

  private refreshBackendUsage(force = false): void {
    const host = this.getSelectedHost();
    if (!host) return;
    for (const kind of USAGE_AWARE_BACKENDS) {
      if (!host.enabled_backends.includes(kind)) continue;
      this.refreshSingleBackendUsage(kind, force);
    }
  }

  private refreshSingleBackendUsage(
    kind: UsageAwareBackendKind,
    force = false,
  ): void {
    if (!this.backendDependencyStatus) return;
    if (this.isUsageBackendMissing(kind)) {
      delete this.backendUsage[kind];
      this.backendUsageError.delete(kind);
      this.backendUsageLoading.delete(kind);
      return;
    }

    if (this.backendUsageLoading.has(kind)) return;
    if (!force && this.backendUsage[kind]) return;

    this.backendUsageLoading.add(kind);
    this.backendUsageError.delete(kind);
    if (this.activeTab === "backends") {
      this.rerenderPanelContent("backends", () => this.buildBackendsContent());
      this.syncProfileDropdown();
    }

    const hostId = this.getSelectedHost()?.id;
    queryBackendUsageBridge(kind, hostId)
      .then((usage) => {
        this.backendUsage[kind] = usage;
        this.backendUsageError.delete(kind);
      })
      .catch((err) => {
        const message = err instanceof Error ? err.message : String(err);
        delete this.backendUsage[kind];
        this.backendUsageError.set(kind, message);
        console.error(`Failed to query usage for backend "${kind}":`, err);
      })
      .finally(() => {
        this.backendUsageLoading.delete(kind);
        if (this.activeTab === "backends") {
          this.rerenderPanelContent("backends", () =>
            this.buildBackendsContent(),
          );
          this.syncProfileDropdown();
        }
      });
  }

  private isUsageBackendMissing(kind: UsageAwareBackendKind): boolean {
    const host = this.getSelectedHost();
    if (host && !host.is_local) return false;
    const dep = this.backendDependencyStatus?.[kind];
    return dep !== undefined && !dep.available;
  }

  private buildBackendUsageSection(kind: BackendKind): HTMLElement | null {
    if (kind === "tycode") return null;
    if (kind === "gemini") return this.buildGeminiUsageNoticeSection();
    if (kind === "claude") return this.buildClaudeUsageNoticeSection();
    const usageKind = kind as UsageAwareBackendKind;
    if (this.isUsageBackendMissing(usageKind)) return null;

    const section = el("div", { class: "settings-backend-usage" });
    const header = el("div", { class: "settings-backend-usage-header" });
    header.appendChild(
      el("span", { class: "settings-backend-usage-title" }, "Usage"),
    );
    section.appendChild(header);

    const usage = this.backendUsage[usageKind];
    if (usage?.plan || usage?.status) {
      const parts: string[] = [];
      if (usage.plan) parts.push(`Plan: ${usage.plan}`);
      if (usage.status) parts.push(`Status: ${usage.status}`);
      section.appendChild(
        el(
          "p",
          { class: "settings-description settings-usage-meta" },
          parts.join(" | "),
        ),
      );
    }

    const windows = usage?.windows ?? [];
    if (windows.length > 0) {
      for (const window of windows) {
        section.appendChild(this.buildBackendUsageWindow(window));
      }
    } else if (this.backendUsageLoading.has(usageKind)) {
      section.appendChild(
        el(
          "p",
          { class: "settings-description settings-usage-meta" },
          "Loading usage limits...",
        ),
      );
    } else if (usage) {
      section.appendChild(
        el(
          "p",
          { class: "settings-description settings-usage-meta" },
          "No usage limits reported.",
        ),
      );
    } else {
      section.appendChild(
        el(
          "p",
          { class: "settings-description settings-usage-meta" },
          "Click Refresh usage to load usage limits.",
        ),
      );
    }

    if (usage?.details?.length) {
      for (const detail of usage.details) {
        section.appendChild(
          el(
            "p",
            { class: "settings-description settings-usage-meta" },
            detail,
          ),
        );
      }
    }

    if (this.backendUsageLoading.has(usageKind) && usage) {
      section.appendChild(
        el(
          "p",
          { class: "settings-description settings-usage-meta" },
          "Refreshing usage...",
        ),
      );
    }

    const error = this.backendUsageError.get(usageKind);
    if (error) {
      section.appendChild(
        el("p", { class: "settings-description settings-usage-error" }, error),
      );
    }

    return section;
  }

  private buildGeminiUsageNoticeSection(): HTMLElement {
    const section = el("div", { class: "settings-backend-usage" });
    const header = el("div", { class: "settings-backend-usage-header" });
    header.appendChild(
      el("span", { class: "settings-backend-usage-title" }, "Usage"),
    );
    section.appendChild(header);

    section.appendChild(
      el(
        "p",
        {
          class: "settings-description settings-usage-meta settings-usage-note",
        },
        "Gemini CLI does not programmatically expose usage limits or provide a usage dashboard.",
      ),
    );

    return section;
  }

  private buildClaudeUsageNoticeSection(): HTMLElement {
    const section = el("div", { class: "settings-backend-usage" });
    const header = el("div", { class: "settings-backend-usage-header" });
    header.appendChild(
      el("span", { class: "settings-backend-usage-title" }, "Usage"),
    );
    section.appendChild(header);

    const usageLink = el(
      "a",
      {
        class: "settings-usage-link",
        href: "https://claude.ai/settings/usage",
        target: "_blank",
        rel: "noopener noreferrer",
      },
      "View Claude usage",
    );
    section.appendChild(usageLink);

    const issueText = el("p", {
      class: "settings-description settings-usage-meta settings-usage-note",
    });
    issueText.append(
      "Claude Code does not programmatically expose usage limits yet. ",
    );
    const issueLink = el(
      "a",
      {
        class: "settings-usage-link",
        href: "https://github.com/anthropics/claude-code/issues/13585",
        target: "_blank",
        rel: "noopener noreferrer",
      },
      "Tracking issue",
    );
    issueText.appendChild(issueLink);
    section.appendChild(issueText);

    return section;
  }

  private buildBackendUsageWindow(window: BackendUsageWindow): HTMLElement {
    const row = el("div", { class: "settings-usage-row" });
    const header = el("div", { class: "settings-usage-row-header" });
    header.appendChild(
      el("span", { class: "settings-usage-label" }, window.label || "Usage"),
    );
    header.appendChild(
      el(
        "span",
        { class: "settings-usage-value" },
        this.formatUsagePercent(window.used_percent),
      ),
    );
    row.appendChild(header);

    const bar = el("div", { class: "settings-usage-bar" });
    const fill = el("div", { class: "settings-usage-bar-fill" });
    const width = this.clampUsagePercent(window.used_percent);
    fill.style.width = `${width}%`;
    bar.appendChild(fill);
    row.appendChild(bar);

    const resetText = this.formatUsageReset(window);
    if (resetText) {
      row.appendChild(
        el(
          "p",
          { class: "settings-description settings-usage-meta" },
          `Resets: ${resetText}`,
        ),
      );
    }
    return row;
  }

  private formatUsagePercent(percent: number | null): string {
    if (percent === null || !Number.isFinite(percent)) return "n/a";
    const rounded = Math.round(percent * 10) / 10;
    if (Math.abs(rounded - Math.round(rounded)) < 0.05) {
      return `${Math.round(rounded)}%`;
    }
    return `${rounded.toFixed(1)}%`;
  }

  private clampUsagePercent(percent: number | null): number {
    if (percent === null || !Number.isFinite(percent)) return 0;
    return Math.max(0, Math.min(100, percent));
  }

  private formatUsageReset(window: BackendUsageWindow): string | null {
    const text = window.reset_at_text?.trim();
    if (text) return text;

    if (
      window.reset_at_unix !== null &&
      Number.isFinite(window.reset_at_unix)
    ) {
      const timestampMs =
        window.reset_at_unix > 1_000_000_000_000
          ? window.reset_at_unix
          : window.reset_at_unix * 1000;
      const date = new Date(timestampMs);
      if (!Number.isNaN(date.getTime())) {
        return date.toLocaleString();
      }
    }

    return null;
  }

  // ========== NOTIFICATIONS TAB ==========

  private buildNotificationsPanel(): HTMLElement {
    const section = el("section", {
      class: "tab-panel",
      "data-panel": "notifications",
      role: "tabpanel",
      "data-testid": "settings-tab-panel",
    });
    section.appendChild(el("h2", { class: "tab-title" }, "Notifications"));

    const popupSection = el("div", { class: "settings-section" });
    popupSection.appendChild(
      el("h3", { class: "settings-section-header" }, "Popup Notifications"),
    );

    const field = el("div", { class: "settings-field" });
    const row = el("div", { class: "settings-toggle-row" });
    const labelCol = el("div", { class: "settings-toggle-label-col" });
    labelCol.appendChild(
      el(
        "label",
        { class: "settings-label", "data-testid": "settings-label" },
        "Show popup notifications",
      ),
    );
    labelCol.appendChild(
      el(
        "p",
        { class: "settings-description" },
        "When disabled, notifications are still available via the bell icon in the toolbar.",
      ),
    );
    row.appendChild(labelCol);

    const toggle = el("label", { class: "settings-toggle" });
    const input = el("input", {
      type: "checkbox",
      "data-testid": "settings-notifications-enabled",
    }) as HTMLInputElement;
    input.checked = this.notificationManager?.isEnabled() ?? true;
    input.addEventListener("change", () => {
      this.notificationManager?.setEnabled(input.checked);
    });
    toggle.appendChild(input);
    toggle.appendChild(el("span", { class: "settings-toggle-slider" }));
    row.appendChild(toggle);

    field.appendChild(row);
    popupSection.appendChild(field);
    section.appendChild(popupSection);

    return section;
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
    const selectedHost = this.getSelectedHost();
    if (!selectedHost) {
      frag.appendChild(
        el("p", { class: "settings-description" }, "No hosts available."),
      );
      return frag;
    }

    frag.appendChild(this.buildDefaultBackendSection());
    const toolbar = el("div", { class: "settings-backend-toolbar" });
    toolbar.appendChild(
      el(
        "p",
        {
          class: "settings-description settings-backend-toolbar-note",
        },
        "Usage is shown for Codex and Kiro.",
      ),
    );
    const refreshAllBtn = el(
      "button",
      {
        class: "settings-usage-refresh settings-usage-refresh-all",
        type: "button",
      },
      this.backendUsageLoading.size > 0 ? "Refreshing..." : "Refresh usage",
    ) as HTMLButtonElement;
    refreshAllBtn.disabled = this.backendUsageLoading.size > 0;
    refreshAllBtn.addEventListener("click", () =>
      this.refreshBackendUsage(true),
    );
    toolbar.appendChild(refreshAllBtn);
    frag.appendChild(toolbar);
    const enabledPrefs = selectedHost.enabled_backends;

    const backends: { kind: BackendKind; label: string; binary: string }[] = [
      { kind: "tycode", label: "Tycode", binary: "tycode-subprocess" },
      { kind: "codex", label: "Codex", binary: "codex" },
      { kind: "claude", label: "Claude Code", binary: "claude" },
      { kind: "kiro", label: "Kiro", binary: "kiro-cli" },
      { kind: "gemini", label: "Gemini", binary: "gemini" },
    ];
    const backendDescriptions: Record<BackendKind, string> = {
      tycode: "Built-in Tyde backend.",
      codex: "OpenAI Codex CLI backend.",
      claude: "Anthropic Claude Code CLI backend.",
      kiro: "Kiro CLI backend.",
      gemini: "Google Gemini CLI backend.",
    };

    const list = el("div", { class: "settings-backend-list" });

    for (const { kind, label, binary } of backends) {
      const card = el("div", { class: "settings-backend-card" });
      const row = el("div", {
        class: "settings-toggle-row settings-backend-card-header",
      });
      const labelCol = el("div", {
        class: "settings-toggle-label-col settings-backend-card-label-col",
      });

      labelCol.appendChild(
        el(
          "label",
          {
            class: "settings-label settings-backend-name",
            "data-testid": "settings-label",
          },
          label,
        ),
      );
      labelCol.appendChild(
        el(
          "p",
          { class: "settings-description settings-backend-subtitle" },
          backendDescriptions[kind],
        ),
      );
      row.appendChild(labelCol);

      const toggle = el("label", { class: "settings-toggle" });
      const input = el("input", {
        type: "checkbox",
        "data-testid": `settings-backend-${kind}-enabled`,
      }) as HTMLInputElement;
      input.checked = enabledPrefs.includes(kind);
      input.addEventListener("change", () => {
        const current = [...selectedHost.enabled_backends];
        if (input.checked) {
          if (!current.includes(kind)) current.push(kind);
        } else {
          const idx = current.indexOf(kind);
          if (idx !== -1) current.splice(idx, 1);
        }
        updateHostEnabledBackendsBridge(selectedHost.id, current)
          .then(() => {
            this.refreshHosts();
            this.onBackendsChanged?.();
          })
          .catch((err) =>
            console.error("Failed to update host backends:", err),
          );
      });
      toggle.appendChild(input);
      toggle.appendChild(el("span", { class: "settings-toggle-slider" }));
      row.appendChild(toggle);
      card.appendChild(row);

      const dep = this.backendDependencyStatus?.[kind];
      const depMissing =
        selectedHost.is_local && dep !== undefined && !dep.available;
      if (depMissing) {
        card.appendChild(
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
          this.syncProfileDropdown();
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
              this.syncProfileDropdown();
            });
        });
        card.appendChild(installBtn);

        if (installError) {
          card.appendChild(
            el(
              "p",
              { class: "settings-description settings-backend-warning" },
              installError,
            ),
          );
        }
      }

      const usageSection = this.buildBackendUsageSection(kind);
      if (usageSection) {
        card.appendChild(usageSection);
      }
      list.appendChild(card);
    }

    frag.appendChild(list);
    return frag;
  }

  // ========== HOST MANAGEMENT ==========

  refreshHosts(): void {
    this.hostsLoading = true;
    listHostsBridge()
      .then((hosts) => {
        this.hosts = hosts;
        this.ensureSelectedHost(true);
        this.refreshRemoteTydeServerStatusForSelectedHost();
      })
      .catch((err) => {
        console.error("Failed to load hosts:", err);
      })
      .finally(() => {
        this.hostsLoading = false;
        this.rerenderHostToolbar();
        this.rerenderPanelContent("backends", () =>
          this.buildBackendsContent(),
        );
        this.rerenderPanelContent("tyde", () => this.buildTydeContent());
        this.syncProfileDropdown();
        this.onHostsUpdated?.();
      });
  }

  private showAddHostModal(): void {
    const fields: [string, string, string][] = [
      ["label", "Display Name", ""],
      ["hostname", "SSH Hostname (e.g. user@server.com)", ""],
    ];
    let selectedKind: "ssh_pipe" | "tyde_server" = "ssh_pipe";
    this.showGenericModal(
      "Add Remote Host",
      fields,
      (values) => {
        const label = values.label.trim();
        const hostname = values.hostname.trim();
        if (!label || !hostname) return;
        addHostBridge(label, hostname, selectedKind)
          .then((host) => {
            this.selectedHostId = host.id;
            saveSelectedHostId(host.id);
            this.refreshHosts();
          })
          .catch((err) => console.error("Failed to add host:", err));
      },
      (form) => {
        const row = el("div", {
          class: "settings-field",
          style: "margin-top: 12px",
        });
        row.appendChild(
          el("label", { class: "settings-label" }, "Remote Type"),
        );
        const segmentRow = el("div", {
          style: "display: flex; gap: 8px; margin-top: 4px",
        });
        const makeSeg = (label: string, value: "ssh_pipe" | "tyde_server") => {
          const btn = el(
            "button",
            {
              class: `segment-btn${value === selectedKind ? " active" : ""}`,
              style:
                "padding: 4px 12px; border: 1px solid var(--border); border-radius: 4px; background: var(--bg-secondary); cursor: pointer; font-size: 12px;",
            },
            label,
          ) as HTMLButtonElement;
          btn.addEventListener("click", (e) => {
            e.preventDefault();
            selectedKind = value;
            segmentRow.querySelectorAll("button").forEach((b) => {
              b.style.background = "var(--bg-secondary)";
              b.style.fontWeight = "normal";
            });
            btn.style.background = "var(--accent)";
            btn.style.fontWeight = "bold";
          });
          if (value === selectedKind) {
            btn.style.background = "var(--accent)";
            btn.style.fontWeight = "bold";
          }
          return btn;
        };
        segmentRow.appendChild(makeSeg("SSH Pipe", "ssh_pipe"));
        segmentRow.appendChild(makeSeg("Tyde Server", "tyde_server"));
        row.appendChild(segmentRow);
        form.appendChild(row);
      },
    );
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
    if (this.adminId === null) {
      frag.appendChild(
        el(
          "p",
          { class: "settings-description" },
          "Open a workspace on the selected host to configure Tycode settings.",
        ),
      );
      return frag;
    }
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
    if (this.adminId === null) {
      frag.appendChild(
        el(
          "p",
          { class: "settings-description" },
          "Open a workspace on the selected host to configure providers.",
        ),
      );
      return frag;
    }
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

  // ========== AGENTS TAB ==========

  private buildAgentsPanel(): HTMLElement {
    const section = el("section", {
      class: "tab-panel",
      "data-panel": "agents",
      role: "tabpanel",
      "data-testid": "settings-tab-panel",
    });
    section.appendChild(el("h2", { class: "tab-title" }, "Agents"));
    section.appendChild(this.buildAgentsContent());
    return section;
  }

  private buildAgentsContent(): DocumentFragment {
    const frag = document.createDocumentFragment();

    const description = el(
      "p",
      { class: "settings-description" },
      "Agent definitions are reusable templates with custom instructions, MCP servers, tool policies, and a default backend. Spawn an agent to start a conversation using its configuration.",
    );
    frag.appendChild(description);

    if (!this.agentDefinitionStore) {
      frag.appendChild(
        el(
          "p",
          { class: "settings-description" },
          this.agentDefinitionUnavailableReason ??
            "Agent definitions are unavailable for this host.",
        ),
      );
      return frag;
    }

    const list = el("div", { class: "settings-card-list" });
    const definitions = this.agentDefinitionStore.getAll();
    for (const def of definitions) {
      list.appendChild(this.buildAgentDefCard(def));
    }
    frag.appendChild(list);

    const addBtn = el(
      "button",
      { class: "settings-add-btn settings-add-btn-primary" },
      "+ New Agent",
    );
    addBtn.dataset.testid = "agents-add-btn";
    addBtn.addEventListener("click", () => this.showAgentDefModal(null));
    frag.appendChild(addBtn);

    return frag;
  }

  private buildAgentDefCard(def: AgentDefinitionEntry): HTMLElement {
    const card = el("div", { class: "settings-provider-card" });
    card.dataset.testid = `agent-def-card-${def.id}`;

    const info = el("div", { class: "settings-provider-info" });
    const header = el("div", { class: "settings-provider-header" });
    header.appendChild(
      el(
        "span",
        {
          class: "settings-provider-name",
          "data-testid": "settings-card-name",
        },
        def.name,
      ),
    );

    const scopeClass =
      def.scope === "builtin"
        ? "status-connected"
        : def.scope === "project"
          ? "status-missing_key"
          : "status-error";
    header.appendChild(
      el("span", { class: `settings-status-chip ${scopeClass}` }, def.scope),
    );

    if (def.default_backend) {
      header.appendChild(
        el(
          "span",
          { class: "settings-status-chip agent-def-backend-chip" },
          def.default_backend,
        ),
      );
    }
    info.appendChild(header);

    if (def.description) {
      info.appendChild(
        el("div", { class: "settings-provider-detail" }, def.description),
      );
    }

    const details: string[] = [];
    if (def.instructions) details.push("Has instructions");
    const mcpCount = def.mcp_servers?.length ?? 0;
    if (mcpCount > 0)
      details.push(`${mcpCount} MCP server${mcpCount > 1 ? "s" : ""}`);
    const skillNameCount = def.skill_names?.length ?? 0;
    if (skillNameCount > 0)
      details.push(`${skillNameCount} skill${skillNameCount > 1 ? "s" : ""}`);
    if (def.tool_policy?.mode && def.tool_policy.mode !== "Unrestricted")
      details.push(`Tools: ${def.tool_policy.mode}`);
    if (def.include_agent_control) details.push("Agent control");
    if (details.length > 0) {
      info.appendChild(
        el("div", { class: "settings-provider-expanded" }, details.join(" · ")),
      );
    }
    card.appendChild(info);

    const actions = el("div", { class: "settings-provider-actions" });

    const spawnWrap = el("div", { class: "agent-def-spawn-split" });
    const spawnBtn = el(
      "button",
      {
        class: "settings-action-btn agent-def-spawn-btn",
        title: `Spawn a new ${def.name} conversation`,
      },
      "Spawn",
    );
    spawnBtn.dataset.testid = `agent-def-spawn-${def.id}`;
    spawnBtn.addEventListener("click", () => {
      const selectedHost = this.getSelectedHost();
      this.onSpawnAgent?.(
        def.id,
        (def.default_backend as BackendKind) || undefined,
        selectedHost,
      );
    });

    const spawnMenuBtn = el(
      "button",
      {
        class: "settings-action-btn agent-def-spawn-menu-btn",
        title: "Choose backend",
      },
      "\u25BE",
    );
    spawnMenuBtn.dataset.testid = `agent-def-spawn-menu-${def.id}`;

    const spawnMenu = el("div", { class: "agent-def-spawn-menu" });
    spawnMenu.hidden = true;

    const backendLabels: Record<string, string> = {
      tycode: "Tycode",
      codex: "Codex",
      claude: "Claude",
      kiro: "Kiro",
      gemini: "Gemini",
    };

    const dismissMenu = () => {
      spawnMenu.hidden = true;
      document.removeEventListener("click", dismissMenu);
    };

    spawnMenuBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      if (!spawnMenu.hidden) {
        dismissMenu();
        return;
      }
      spawnMenu.innerHTML = "";
      const host = this.getSelectedHost();
      const enabledBackends = host?.enabled_backends ?? [];
      for (const kind of enabledBackends) {
        const label = backendLabels[kind] ?? kind;
        const opt = el(
          "button",
          { class: "agent-def-spawn-menu-item" },
          `Spawn with ${label}`,
        );
        opt.addEventListener("click", () => {
          dismissMenu();
          const selectedHost = this.getSelectedHost();
          this.onSpawnAgent?.(def.id, kind as BackendKind, selectedHost);
        });
        spawnMenu.appendChild(opt);
      }
      spawnMenu.hidden = false;
      requestAnimationFrame(() =>
        document.addEventListener("click", dismissMenu),
      );
    });

    spawnWrap.appendChild(spawnBtn);
    spawnWrap.appendChild(spawnMenuBtn);
    spawnWrap.appendChild(spawnMenu);
    actions.appendChild(spawnWrap);

    const editBtn = el(
      "button",
      { class: "settings-action-btn", title: "Edit agent" },
      "Edit",
    );
    editBtn.addEventListener("click", () => this.showAgentDefModal(def));
    actions.appendChild(editBtn);

    if (def.scope !== "builtin") {
      const delBtn = el(
        "button",
        {
          class: "settings-action-btn settings-provider-delete",
          title: "Delete agent",
        },
        "Delete",
      );
      delBtn.addEventListener("click", () => {
        void this.agentDefinitionStore?.delete(def.id).then(() => {
          this.rerenderPanelContent("agents", () => this.buildAgentsContent());
        });
      });
      actions.appendChild(delBtn);
    }

    card.appendChild(actions);
    return card;
  }

  private showAgentDefModal(existing: AgentDefinitionEntry | null): void {
    if (!this.agentDefinitionStore) return;

    const isNew = existing === null;
    const title = isNew ? "New Agent" : `Edit ${existing.name}`;

    const overlay = el("div", { class: "settings-modal-overlay" });
    const modal = el("div", { class: "settings-modal agent-def-modal" });
    modal.appendChild(el("h3", {}, title));

    // Name
    const nameField = el("div", { class: "settings-field" });
    nameField.appendChild(el("label", { class: "settings-label" }, "Name"));
    const nameInput = el("input", {
      class: "settings-input",
      type: "text",
      placeholder: "e.g. Code Reviewer",
    }) as HTMLInputElement;
    nameInput.value = existing?.name ?? "";
    nameField.appendChild(nameInput);
    modal.appendChild(nameField);

    // Description
    const descField = el("div", { class: "settings-field" });
    descField.appendChild(
      el("label", { class: "settings-label" }, "Description"),
    );
    const descInput = el("input", {
      class: "settings-input",
      type: "text",
      placeholder: "Short description of what this agent does",
    }) as HTMLInputElement;
    descInput.value = existing?.description ?? "";
    descField.appendChild(descInput);
    modal.appendChild(descField);

    // Instructions
    const instrField = el("div", {
      class: "settings-field agent-def-instructions-field",
    });
    instrField.appendChild(
      el("label", { class: "settings-label" }, "Instructions"),
    );
    const instrDesc = el(
      "p",
      { class: "settings-description" },
      "Custom system instructions prepended to every conversation with this agent.",
    );
    instrField.appendChild(instrDesc);
    const instrInput = el("textarea", {
      class: "settings-textarea",
      rows: "4",
      placeholder:
        "You are a code reviewer. Focus on correctness, security, and readability...",
    }) as HTMLTextAreaElement;
    instrInput.value = existing?.instructions ?? "";
    instrField.appendChild(instrInput);
    modal.appendChild(instrField);

    // Default Backend
    const backendField = el("div", { class: "settings-field" });
    backendField.appendChild(
      el("label", { class: "settings-label" }, "Default Backend"),
    );
    const backendSelect = el("select", {
      class: "settings-select",
    }) as HTMLSelectElement;
    const backendOptions = [
      ["", "None (use workspace default)"],
      ["tycode", "Tycode"],
      ["claude", "Claude"],
      ["codex", "Codex"],
      ["kiro", "Kiro"],
      ["gemini", "Gemini"],
    ];
    for (const [value, label] of backendOptions) {
      const opt = el("option", { value }, label);
      if ((existing?.default_backend ?? "") === value) {
        opt.selected = true;
      }
      backendSelect.appendChild(opt);
    }
    backendField.appendChild(backendSelect);
    modal.appendChild(backendField);

    // Include Agent Control toggle
    const controlField = el("div", { class: "settings-field" });
    const controlRow = el("div", { class: "settings-toggle-row" });
    const controlLabelCol = el("div", { class: "settings-toggle-label-col" });
    controlLabelCol.appendChild(
      el("label", { class: "settings-label" }, "Include Agent Control"),
    );
    controlLabelCol.appendChild(
      el(
        "p",
        { class: "settings-description" },
        "Give this agent access to spawn and manage other agents.",
      ),
    );
    controlRow.appendChild(controlLabelCol);
    const controlToggle = el("label", { class: "settings-toggle" });
    const controlInput = el("input", { type: "checkbox" }) as HTMLInputElement;
    controlInput.checked = existing?.include_agent_control ?? false;
    controlToggle.appendChild(controlInput);
    controlToggle.appendChild(el("span", { class: "settings-toggle-slider" }));
    controlRow.appendChild(controlToggle);
    controlField.appendChild(controlRow);
    modal.appendChild(controlField);

    // MCP Servers
    const mcpServers: AgentMcpServer[] = [...(existing?.mcp_servers ?? [])];

    const mcpField = el("div", {
      class: "settings-field agent-def-mcp-field",
    });
    mcpField.appendChild(
      el("label", { class: "settings-label" }, "MCP Servers"),
    );
    mcpField.appendChild(
      el(
        "p",
        { class: "settings-description" },
        "Custom MCP servers available only to this agent.",
      ),
    );

    const mcpList = el("div", { class: "agent-def-mcp-list" });

    const renderMcpList = () => {
      mcpList.innerHTML = "";
      for (let i = 0; i < mcpServers.length; i++) {
        const srv = mcpServers[i];
        const isHttp = srv.transport.type === "http";
        const detail = isHttp
          ? (srv.transport as any).url
          : (srv.transport as any).command;

        const row = el("div", { class: "agent-def-mcp-row" });
        const info = el("div", { class: "agent-def-mcp-row-info" });
        info.appendChild(
          el("span", { class: "agent-def-mcp-row-name" }, srv.name),
        );
        info.appendChild(
          el(
            "span",
            { class: "agent-def-mcp-row-detail" },
            `${isHttp ? "HTTP" : "Stdio"}: ${detail}`,
          ),
        );
        row.appendChild(info);

        const rowActions = el("div", { class: "agent-def-mcp-row-actions" });
        const editBtn = el(
          "button",
          { class: "agent-def-mcp-row-btn", title: "Edit" },
          "Edit",
        );
        editBtn.addEventListener("click", () => {
          this.showAgentMcpServerModal(srv, (updated) => {
            mcpServers[i] = updated;
            renderMcpList();
          });
        });
        const removeBtn = el(
          "button",
          {
            class: "agent-def-mcp-row-btn agent-def-mcp-row-remove",
            title: "Remove",
          },
          "Remove",
        );
        removeBtn.addEventListener("click", () => {
          mcpServers.splice(i, 1);
          renderMcpList();
        });
        rowActions.appendChild(editBtn);
        rowActions.appendChild(removeBtn);
        row.appendChild(rowActions);
        mcpList.appendChild(row);
      }
    };
    renderMcpList();

    mcpField.appendChild(mcpList);

    const addMcpBtn = el(
      "button",
      { class: "settings-add-btn agent-def-mcp-add" },
      "+ Add Server",
    );
    addMcpBtn.addEventListener("click", () => {
      this.showAgentMcpServerModal(null, (srv) => {
        mcpServers.push(srv);
        renderMcpList();
      });
    });
    mcpField.appendChild(addMcpBtn);
    modal.appendChild(mcpField);

    // Skills (names from ~/.tyde/skills/)
    const skillNames: string[] = [...(existing?.skill_names ?? [])];

    const skillNamesField = el("div", {
      class: "settings-field agent-def-mcp-field",
    });
    skillNamesField.appendChild(
      el("label", { class: "settings-label" }, "Skills"),
    );
    skillNamesField.appendChild(
      el(
        "p",
        { class: "settings-description" },
        "Skills from ~/.tyde/skills/ to load for this agent.",
      ),
    );

    const skillNamesList = el("div", { class: "agent-def-mcp-list" });

    const renderSkillNamesList = () => {
      skillNamesList.innerHTML = "";
      for (let i = 0; i < skillNames.length; i++) {
        const sn = skillNames[i];

        const row = el("div", { class: "agent-def-mcp-row" });
        const info = el("div", { class: "agent-def-mcp-row-info" });
        info.appendChild(el("span", { class: "agent-def-mcp-row-name" }, sn));
        row.appendChild(info);

        const rowActions = el("div", { class: "agent-def-mcp-row-actions" });
        const removeBtn = el(
          "button",
          {
            class: "agent-def-mcp-row-btn agent-def-mcp-row-remove",
            title: "Remove",
          },
          "Remove",
        );
        removeBtn.addEventListener("click", () => {
          skillNames.splice(i, 1);
          renderSkillNamesList();
        });
        rowActions.appendChild(removeBtn);
        row.appendChild(rowActions);
        skillNamesList.appendChild(row);
      }
    };
    renderSkillNamesList();

    skillNamesField.appendChild(skillNamesList);

    const addSkillNameBtn = el(
      "button",
      { class: "settings-add-btn agent-def-mcp-add" },
      "+ Add Skill",
    );
    addSkillNameBtn.addEventListener("click", async () => {
      const inputRow = el("div", { class: "agent-def-skill-path-input-row" });

      // Load available skills for a dropdown.
      let availableSkills: string[] = [];
      let loadError: string | null = null;
      try {
        availableSkills = await invoke("list_available_skills");
      } catch (err) {
        loadError = String(err);
        console.error("Failed to list available skills:", err);
      }

      // Show error or info text.
      if (loadError) {
        inputRow.appendChild(
          el(
            "p",
            {
              class: "settings-description",
              style: "color: var(--error-color, #e55)",
            },
            `Failed to load available skills: ${loadError}`,
          ),
        );
      } else if ((availableSkills || []).length === 0) {
        inputRow.appendChild(
          el(
            "p",
            { class: "settings-description" },
            "No skills found in ~/.tyde/skills/",
          ),
        );
      }

      // Filter out already-added skills.
      const remaining = (availableSkills || []).filter(
        (s: string) => !skillNames.includes(s),
      );

      if (remaining.length > 0) {
        const selectEl = el("select", {
          class: "settings-select",
        }) as HTMLSelectElement;
        selectEl.appendChild(el("option", { value: "" }, "Select a skill..."));
        for (const name of remaining) {
          selectEl.appendChild(el("option", { value: name }, name));
        }
        inputRow.appendChild(selectEl);

        const confirmBtn = el(
          "button",
          { class: "settings-action-btn" },
          "Add",
        );
        const addSelectedSkill = () => {
          const val = selectEl.value;
          if (val && !skillNames.includes(val)) {
            skillNames.push(val);
            renderSkillNamesList();
          }
          inputRow.remove();
        };
        confirmBtn.addEventListener("click", addSelectedSkill);
        inputRow.appendChild(confirmBtn);
        selectEl.addEventListener("change", addSelectedSkill);
      } else {
        // Manual entry.
        const nameInput = el("input", {
          class: "settings-input",
          type: "text",
          placeholder: "Skill name",
        }) as HTMLInputElement;
        inputRow.appendChild(nameInput);

        const confirmBtn = el(
          "button",
          { class: "settings-action-btn" },
          "Add",
        );
        confirmBtn.addEventListener("click", () => {
          const val = nameInput.value.trim();
          if (val) {
            skillNames.push(val);
            renderSkillNamesList();
          }
          inputRow.remove();
        });
        inputRow.appendChild(confirmBtn);

        nameInput.addEventListener("keydown", (e) => {
          if (e.key === "Enter") confirmBtn.click();
          else if (e.key === "Escape") inputRow.remove();
        });
      }

      skillNamesField.insertBefore(inputRow, addSkillNameBtn);
    });
    skillNamesField.appendChild(addSkillNameBtn);
    modal.appendChild(skillNamesField);

    // Actions
    const actions = el("div", { class: "settings-modal-actions" });
    const cancelBtn = el("button", {}, "Cancel");
    cancelBtn.addEventListener("click", () => overlay.remove());

    const saveBtn = el(
      "button",
      { class: "settings-modal-save" },
      isNew ? "Create" : "Save",
    );
    saveBtn.addEventListener("click", () => {
      const name = nameInput.value.trim();
      if (!name) {
        nameInput.focus();
        return;
      }

      const entry: AgentDefinitionEntry = {
        id: existing?.id ?? name.toLowerCase().replace(/[^a-z0-9]+/g, "-"),
        name,
        description: descInput.value.trim(),
        instructions: instrInput.value.trim() || undefined,
        bootstrap_prompt: existing?.bootstrap_prompt,
        skill_names: skillNames.length > 0 ? skillNames : undefined,
        mcp_servers: mcpServers,
        tool_policy: existing?.tool_policy ?? { mode: "Unrestricted" },
        default_backend: backendSelect.value || undefined,
        include_agent_control: controlInput.checked,
        builtin: false,
        scope: existing?.scope === "project" ? "project" : "global",
      };

      overlay.remove();
      void this.agentDefinitionStore
        ?.save(entry, entry.scope === "project" ? "project" : "global")
        .then(() => {
          this.rerenderPanelContent("agents", () => this.buildAgentsContent());
        });
    });

    actions.appendChild(cancelBtn);
    actions.appendChild(saveBtn);
    modal.appendChild(actions);

    overlay.appendChild(modal);
    overlay.addEventListener("click", (e) => {
      if (e.target === overlay) overlay.remove();
    });
    document.body.appendChild(overlay);
    nameInput.focus();
  }

  private showAgentMcpServerModal(
    existing: AgentMcpServer | null,
    onSave: (server: AgentMcpServer) => void,
  ): void {
    const isHttp = existing ? existing.transport.type === "http" : false;

    const overlay = el("div", { class: "settings-modal-overlay" });
    const modal = el("div", { class: "settings-modal" });
    modal.appendChild(
      el("h3", {}, existing ? "Edit MCP Server" : "Add MCP Server"),
    );

    // Name
    const nameField = el("div", { class: "settings-field" });
    nameField.appendChild(el("label", { class: "settings-label" }, "Name"));
    const nameInput = el("input", {
      class: "settings-input",
      type: "text",
      placeholder: "e.g. ops_mcp",
    }) as HTMLInputElement;
    nameInput.value = existing?.name ?? "";
    nameField.appendChild(nameInput);
    modal.appendChild(nameField);

    // Transport type selector
    const typeField = el("div", { class: "settings-field" });
    typeField.appendChild(
      el("label", { class: "settings-label" }, "Transport"),
    );
    const typeSelect = el("select", {
      class: "settings-select",
    }) as HTMLSelectElement;
    for (const [value, label] of [
      ["stdio", "Stdio"],
      ["http", "HTTP"],
    ]) {
      const opt = el("option", { value }, label);
      if ((value === "http" && isHttp) || (value === "stdio" && !isHttp)) {
        opt.selected = true;
      }
      typeSelect.appendChild(opt);
    }
    typeField.appendChild(typeSelect);
    modal.appendChild(typeField);

    // Stdio fields
    const stdioFields = el("div", {
      class: "agent-mcp-transport-fields",
    });

    const cmdField = el("div", { class: "settings-field" });
    cmdField.appendChild(el("label", { class: "settings-label" }, "Command"));
    const cmdInput = el("input", {
      class: "settings-input",
      type: "text",
      placeholder: "e.g. npx",
    }) as HTMLInputElement;
    cmdInput.value =
      existing && existing.transport.type === "stdio"
        ? (existing.transport as any).command
        : "";
    cmdField.appendChild(cmdInput);
    stdioFields.appendChild(cmdField);

    const argsField = el("div", { class: "settings-field" });
    argsField.appendChild(
      el("label", { class: "settings-label" }, "Arguments (one per line)"),
    );
    const argsInput = el("textarea", {
      class: "settings-textarea",
      rows: "3",
    }) as HTMLTextAreaElement;
    argsInput.value =
      existing && existing.transport.type === "stdio"
        ? ((existing.transport as any).args ?? []).join("\n")
        : "";
    argsField.appendChild(argsInput);
    stdioFields.appendChild(argsField);

    const envField = el("div", { class: "settings-field" });
    envField.appendChild(
      el("label", { class: "settings-label" }, "Env Vars (KEY=VALUE per line)"),
    );
    const envInput = el("textarea", {
      class: "settings-textarea",
      rows: "3",
    }) as HTMLTextAreaElement;
    envInput.value =
      existing && existing.transport.type === "stdio"
        ? Object.entries((existing.transport as any).env ?? {})
            .map(([k, v]) => `${k}=${v}`)
            .join("\n")
        : "";
    envField.appendChild(envInput);
    stdioFields.appendChild(envField);
    modal.appendChild(stdioFields);

    // HTTP fields
    const httpFields = el("div", {
      class: "agent-mcp-transport-fields",
    });

    const urlField = el("div", { class: "settings-field" });
    urlField.appendChild(el("label", { class: "settings-label" }, "URL"));
    const urlInput = el("input", {
      class: "settings-input",
      type: "text",
      placeholder: "http://localhost:9000/mcp",
    }) as HTMLInputElement;
    urlInput.value =
      existing && existing.transport.type === "http"
        ? (existing.transport as any).url
        : "";
    urlField.appendChild(urlInput);
    httpFields.appendChild(urlField);

    const headersField = el("div", { class: "settings-field" });
    headersField.appendChild(
      el("label", { class: "settings-label" }, "Headers (KEY=VALUE per line)"),
    );
    const headersInput = el("textarea", {
      class: "settings-textarea",
      rows: "3",
    }) as HTMLTextAreaElement;
    headersInput.value =
      existing && existing.transport.type === "http"
        ? Object.entries((existing.transport as any).headers ?? {})
            .map(([k, v]) => `${k}=${v}`)
            .join("\n")
        : "";
    headersField.appendChild(headersInput);
    httpFields.appendChild(headersField);
    modal.appendChild(httpFields);

    // Show/hide based on transport type
    const updateVisibility = () => {
      const isHttpSelected = typeSelect.value === "http";
      stdioFields.hidden = isHttpSelected;
      httpFields.hidden = !isHttpSelected;
    };
    typeSelect.addEventListener("change", updateVisibility);
    updateVisibility();

    // Actions
    const actions = el("div", { class: "settings-modal-actions" });
    const cancelBtn = el("button", {}, "Cancel");
    cancelBtn.addEventListener("click", () => overlay.remove());

    const saveBtn = el("button", { class: "settings-modal-save" }, "Save");
    saveBtn.addEventListener("click", () => {
      const name = nameInput.value.trim();
      if (!name) {
        nameInput.focus();
        return;
      }

      let server: AgentMcpServer;
      if (typeSelect.value === "http") {
        const url = urlInput.value.trim();
        if (!url) {
          urlInput.focus();
          return;
        }
        const headers: Record<string, string> = {};
        for (const line of headersInput.value.split("\n")) {
          const eq = line.indexOf("=");
          if (eq === -1) continue;
          const k = line.substring(0, eq).trim();
          const v = line.substring(eq + 1).trim();
          if (k) headers[k] = v;
        }
        server = {
          name,
          transport: { type: "http", url, headers },
        } as AgentMcpServer;
      } else {
        const command = cmdInput.value.trim();
        if (!command) {
          cmdInput.focus();
          return;
        }
        const args = argsInput.value
          .split("\n")
          .map((l: string) => l.trim())
          .filter(Boolean);
        const env: Record<string, string> = {};
        for (const line of envInput.value.split("\n")) {
          const eq = line.indexOf("=");
          if (eq === -1) continue;
          const k = line.substring(0, eq).trim();
          const v = line.substring(eq + 1).trim();
          if (k) env[k] = v;
        }
        server = {
          name,
          transport: { type: "stdio", command, args, env },
        } as AgentMcpServer;
      }

      overlay.remove();
      onSave(server);
    });

    actions.appendChild(cancelBtn);
    actions.appendChild(saveBtn);
    modal.appendChild(actions);

    overlay.appendChild(modal);
    overlay.addEventListener("click", (e) => {
      if (e.target === overlay) overlay.remove();
    });
    document.body.appendChild(overlay);
    nameInput.focus();
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
    frag.appendChild(this.buildRemoteTydeServerSection());
    frag.appendChild(this.buildRemoteControlSection());
    return frag;
  }

  private buildRemoteTydeServerSection(): HTMLElement {
    const section = el("div", { class: "settings-section" });
    section.appendChild(
      el("h3", { class: "settings-section-header" }, "Remote Tyde Server"),
    );

    const selectedHost = this.getSelectedHost();
    if (!this.hostUsesRemoteTydeServer(selectedHost)) {
      section.appendChild(
        el(
          "p",
          { class: "settings-description" },
          "Select a Tyde Server host to manage remote install and runtime.",
        ),
      );
      return section;
    }

    section.appendChild(
      el(
        "p",
        { class: "settings-description" },
        `${selectedHost.hostname} · local v${this.remoteTydeServerStatus?.local_version ?? "?"}`,
      ),
    );

    if (this.remoteTydeServerStatusLoading) {
      section.appendChild(
        el(
          "p",
          { class: "settings-description" },
          "Checking remote Tyde server status...",
        ),
      );
    } else if (this.remoteTydeServerStatusError) {
      section.appendChild(
        el(
          "p",
          { class: "settings-description settings-backend-warning" },
          this.remoteTydeServerStatusError,
        ),
      );
    }

    const status =
      this.remoteTydeServerStatus?.host_id === selectedHost.id
        ? this.remoteTydeServerStatus
        : null;

    if (status) {
      const lines: string[] = [];
      switch (status.state) {
        case "not_installed":
          lines.push("Client-matching Tyde is not installed.");
          break;
        case "stopped":
          lines.push("Tyde is installed but not running.");
          break;
        case "running_current":
          lines.push("Tyde server is running and matches this client.");
          break;
        case "running_stale":
          lines.push("Tyde server is running but version differs from client.");
          break;
        case "running_unknown":
          lines.push("Tyde server is running; version could not be verified.");
          break;
        case "error":
          lines.push("Could not fully inspect remote Tyde server.");
          break;
      }
      if (status.target) lines.push(`Target: ${status.target}`);
      if (status.install_path)
        lines.push(`Install path: ${status.install_path}`);
      if (status.socket_path) lines.push(`Socket: ${status.socket_path}`);
      if (status.remote_version) {
        lines.push(`Remote version: v${status.remote_version}`);
      }
      if (
        !status.installed_client_version &&
        status.installed_versions.length > 0
      ) {
        lines.push(
          `Installed versions: ${status.installed_versions
            .map((v) => `v${v}`)
            .join(", ")}`,
        );
      }
      if (status.error) {
        section.appendChild(
          el(
            "p",
            { class: "settings-description settings-backend-warning" },
            status.error,
          ),
        );
      }
      for (const line of lines) {
        section.appendChild(el("p", { class: "settings-description" }, line));
      }
    }

    const actionRow = el("div", {
      style: "display: flex; gap: 8px; flex-wrap: wrap; margin-top: 8px;",
    });
    const busy = this.remoteTydeServerActionLoading !== null;

    const makeActionButton = (
      label: string,
      action: "install" | "launch" | "install_launch" | "upgrade" | "refresh",
      disabled: boolean,
    ) => {
      const actionLoading =
        action !== "refresh" && this.remoteTydeServerActionLoading === action;
      const button = el(
        "button",
        {
          class: "settings-action-btn",
          type: "button",
        },
        actionLoading ? "Working..." : label,
      ) as HTMLButtonElement;
      button.disabled = disabled || busy || this.remoteTydeServerStatusLoading;
      button.addEventListener("click", () => {
        if (action === "refresh") {
          this.refreshRemoteTydeServerStatusForSelectedHost();
          return;
        }
        this.runRemoteTydeServerAction(action);
      });
      return button;
    };

    const statusState = status?.state ?? "error";
    const canInstallAndLaunch =
      statusState === "not_installed" || statusState === "error" || !status;
    const canLaunch =
      !!status && status.installed_client_version && !status.running;
    const canInstall = !status || !status.installed_client_version;
    const canUpgrade = !!status && status.needs_upgrade;

    if (canInstallAndLaunch) {
      actionRow.appendChild(
        makeActionButton("Install & Launch", "install_launch", false),
      );
    }
    if (canLaunch) {
      actionRow.appendChild(makeActionButton("Launch", "launch", false));
    }
    if (canInstall) {
      actionRow.appendChild(makeActionButton("Install", "install", false));
    }
    if (canUpgrade) {
      actionRow.appendChild(makeActionButton("Upgrade", "upgrade", false));
    }
    actionRow.appendChild(makeActionButton("Refresh", "refresh", false));

    section.appendChild(actionRow);
    return section;
  }

  private buildRemoteControlSection(): HTMLElement {
    const section = el("div", { class: "settings-section" });
    section.appendChild(
      el("h3", { class: "settings-section-header" }, "Remote Control"),
    );

    const field = el("div", { class: "settings-field" });
    const row = el("div", { class: "settings-toggle-row" });
    const labelCol = el("div", { class: "settings-toggle-label-col" });
    labelCol.appendChild(
      el("label", { class: "settings-label" }, "Allow Remote Control over SSH"),
    );

    const statusText = this.remoteControlLoading
      ? "Loading..."
      : this.remoteControlSettings.running
        ? `Running (${this.remoteControlSettings.connected_clients} client${this.remoteControlSettings.connected_clients !== 1 ? "s" : ""})`
        : "Stopped";
    labelCol.appendChild(
      el("p", { class: "settings-description" }, statusText),
    );
    if (
      this.remoteControlSettings.running &&
      this.remoteControlSettings.socket_path
    ) {
      labelCol.appendChild(
        el(
          "p",
          { class: "settings-description" },
          this.remoteControlSettings.socket_path,
        ),
      );
    }
    row.appendChild(labelCol);

    const toggle = el("label", { class: "settings-toggle" });
    const input = el("input", {
      type: "checkbox",
      "data-testid": "settings-remote-control-enabled",
    }) as HTMLInputElement;
    input.checked = this.remoteControlSettings.enabled;
    input.disabled = this.remoteControlLoading;
    input.addEventListener("change", () => {
      this.setRemoteControlEnabled(input.checked);
    });
    toggle.appendChild(input);
    toggle.appendChild(el("span", { class: "settings-toggle-slider" }));
    row.appendChild(toggle);
    field.appendChild(row);

    section.appendChild(field);
    return section;
  }

  private setRemoteControlEnabled(enabled: boolean): void {
    this.remoteControlLoading = true;
    this.rerenderPanelContent("tyde", () => this.buildTydeContent());
    setRemoteControlEnabledBridge(enabled)
      .then((settings) => {
        this.remoteControlSettings = settings;
      })
      .catch((err) => {
        console.error("Failed to update remote control setting:", err);
      })
      .finally(() => {
        this.remoteControlLoading = false;
        this.rerenderPanelContent("tyde", () => this.buildTydeContent());
      });
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
    if (this.adminId === null) {
      frag.appendChild(
        el(
          "p",
          { class: "settings-description" },
          "Open a workspace on the selected host to configure MCP servers.",
        ),
      );
      return frag;
    }
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
    if (this.adminId === null) {
      frag.appendChild(
        el(
          "p",
          { class: "settings-description" },
          "Open a workspace on the selected host to configure agent models.",
        ),
      );
      return frag;
    }
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
    if (this.adminId === null) {
      frag.appendChild(
        el(
          "p",
          { class: "settings-description" },
          "Open a workspace on the selected host to configure advanced settings.",
        ),
      );
      return frag;
    }
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
        "Ignore custom steering docs from .tyde and external agent configs.",
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
    const selectedHost = this.getSelectedHost();
    backendSelect.disabled = !selectedHost;
    backendSelect.addEventListener("change", () => {
      const host = this.getSelectedHost();
      if (!host) return;
      const backend = normalizeBackendKind(backendSelect.value);
      updateHostDefaultBackendsBridge(host.id, backend)
        .then(() => this.refreshHosts())
        .catch((err) =>
          console.error("Failed to update host default backend:", err),
        );
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
    (select as HTMLSelectElement).disabled = this.adminId === null;
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
    (refreshBtn as HTMLButtonElement).disabled = this.adminId === null;
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
    (spawnSelect as HTMLSelectElement).disabled = this.adminId === null;
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
      const selectedHost = this.getSelectedHost();
      const enabledBackends = (selectedHost?.enabled_backends ?? []).filter(
        (kind): kind is BackendKind =>
          (ALL_BACKENDS as string[]).includes(kind) &&
          (selectedHost?.is_local
            ? (this.backendDependencyStatus?.[kind as BackendKind]?.available ??
              true)
            : true),
      );
      const backendLabels: Record<BackendKind, string> = {
        tycode: "Tycode",
        codex: "Codex",
        claude: "Claude Code",
        kiro: "Kiro",
        gemini: "Gemini",
      };
      for (const kind of enabledBackends) {
        const opt = el("option", { value: kind }, backendLabels[kind]);
        if (kind === normalizeBackendKind(selectedHost?.default_backend))
          opt.selected = true;
        backendSelect.appendChild(opt);
      }
      if (
        selectedHost &&
        enabledBackends.length > 0 &&
        !enabledBackends.includes(
          normalizeBackendKind(selectedHost.default_backend),
        )
      ) {
        // Correct the host's default backend without cascading a full refresh
        selectedHost.default_backend = enabledBackends[0];
        void updateHostDefaultBackendsBridge(
          selectedHost.id,
          enabledBackends[0],
        ).catch((err) =>
          console.error("Failed to update host default backend:", err),
        );
      }
    }

    if (profileSelect) {
      profileSelect.innerHTML = "";
      profileSelect.disabled = this.adminId === null;
      if (this.adminId === null) {
        const opt = el(
          "option",
          { value: "", disabled: "true" },
          "Not connected",
        );
        opt.selected = true;
        profileSelect.appendChild(opt);
      } else if (this.profiles.length === 0) {
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
    spawnSelect.disabled = this.adminId === null;
    if (this.adminId === null) {
      const opt = el(
        "option",
        { value: "", disabled: "true" },
        "Not connected",
      );
      opt.selected = true;
      spawnSelect.appendChild(opt);
      return;
    }
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
    customizeForm?: (form: HTMLElement) => void,
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

    if (customizeForm) {
      customizeForm(modal);
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
