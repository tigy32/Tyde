import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import {
  type AdminEventPayload,
  type BackendKind,
  type ChatEventPayload,
  type CommandName,
  type CommandParams,
  type CommandResponse,
  type ConversationMode,
  type ConversationRegisteredData,
  type ConversationRegisteredPayload,
  type FileChangedPayload,
  type ImageAttachment,
  ProtocolParseError,
  parseChatEvent,
  type RemoteConnectionProgress,
  type RuntimeAgent,
  type TerminalExitPayload,
  type TerminalOutputPayload,
} from "@tyde/protocol";

export type {
  AdminEventPayload,
  BackendDependencyStatus,
  BackendDepResult,
  BackendKind,
  ChatEventPayload,
  CollectedAgentResult,
  ConversationMode,
  ConversationRegisteredData,
  ConversationRegisteredPayload,
  CreateConversationResponse,
  DriverMcpHttpServerSettings,
  FileChangedPayload,
  McpHttpServerSettings,
  RemoteConnectionProgress,
  RuntimeAgent,
  RuntimeAgentEventBatch,
  SessionRecord,
  ShellCommandResult,
  SpawnAgentResponse,
  TerminalExitPayload,
  TerminalOutputPayload,
  WorkflowEntry,
} from "@tyde/protocol";

export interface Host {
  id: string;
  label: string;
  hostname: string;
  is_local: boolean;
  enabled_backends: string[];
  default_backend: string;
}

export function normalizeBackendKind(
  value: string | null | undefined,
): BackendKind {
  const normalized = (value ?? "").trim().toLowerCase();
  if (normalized === "codex") return "codex";
  if (normalized === "claude" || normalized === "claude_code") return "claude";
  if (normalized === "kiro") return "kiro";
  return "tycode";
}

function friendlyError(raw: string): string {
  const msg = String(raw);
  return msg.length > 200 ? `${msg.slice(0, 200)}…` : msg;
}

async function execute<K extends CommandName>(
  command: K,
  params: CommandParams<K>,
): Promise<CommandResponse<K>> {
  return invoke<CommandResponse<K>>(command, params).catch((err) => {
    console.error(`bridge: ${command} failed:`, err);
    throw new Error(friendlyError(String(err)));
  });
}

// --- Conversation management ---

export function createConversation(
  workspaceRoots: string[],
  backendKind?: BackendKind,
  ephemeral?: boolean,
  conversationMode?: ConversationMode,
) {
  return execute("create_conversation", {
    workspaceRoots,
    backendKind,
    ephemeral,
    conversationMode,
  });
}

export function sendMessage(
  conversationId: number,
  message: string,
  images?: ImageAttachment[],
) {
  return execute("send_message", { conversationId, message, images });
}

export function cancelConversation(conversationId: number) {
  return execute("cancel_conversation", { conversationId });
}

export function closeConversation(conversationId: number) {
  return execute("close_conversation", { conversationId });
}

// --- Sessions ---

export function listSessions(conversationId: number) {
  return execute("list_sessions", { conversationId });
}

export function resumeSession(conversationId: number, sessionId: string) {
  return execute("resume_session", { conversationId, sessionId });
}

export function getSessionId(conversationId: number) {
  return execute("get_session_id", { conversationId });
}

export function deleteSession(conversationId: number, sessionId: string) {
  return execute("delete_session", { conversationId, sessionId });
}

export function exportSessionJson(sessionId: string) {
  return execute("export_session_json", { sessionId });
}

export function listSessionRecords() {
  return execute("list_session_records", {} as Record<string, never>);
}

export function renameSession(id: string, name: string) {
  return execute("rename_session", { id, name });
}

// --- Settings & models ---

export function getSettings(conversationId: number) {
  return execute("get_settings", { conversationId });
}

export function updateSettings(
  conversationId: number,
  settings: Record<string, unknown>,
  persist?: boolean,
) {
  return execute("update_settings", {
    conversationId,
    settings,
    persist: persist ?? false,
  });
}

export function listModels(conversationId: number) {
  return execute("list_models", { conversationId });
}

export function listProfiles(conversationId: number) {
  return execute("list_profiles", { conversationId });
}

export function switchProfile(conversationId: number, profileName: string) {
  return execute("switch_profile", { conversationId, profileName });
}

export function getModuleSchemas(conversationId: number) {
  return execute("get_module_schemas", { conversationId });
}

// --- Agent control ---

export function spawnAgent(
  workspaceRoots: string[],
  prompt: string,
  backendKind?: BackendKind,
  parentAgentId?: number,
  name?: string,
  ephemeral?: boolean,
) {
  return execute("spawn_agent", {
    workspaceRoots,
    prompt,
    backendKind,
    parentAgentId,
    name,
    ephemeral,
  });
}

export function sendAgentMessage(agentId: number, message: string) {
  return execute("send_agent_message", { agentId, message });
}

export function interruptAgent(agentId: number) {
  return execute("interrupt_agent", { agentId });
}

export function terminateAgent(agentId: number) {
  return execute("terminate_agent", { agentId });
}

export function getAgent(agentId: number) {
  return execute("get_agent", { agentId });
}

export function renameAgent(agentId: number, name: string) {
  return execute("rename_agent", { agentId, name });
}

export function listAgents() {
  return execute("list_agents", {} as Record<string, never>);
}

export function waitForAgent(agentId: number, timeoutMs?: number) {
  return execute("wait_for_agent", { agentId, timeoutMs });
}

export function agentEventsSince(sinceSeq = 0, limit = 200) {
  return execute("agent_events_since", { sinceSeq, limit });
}

export function collectAgentResult(agentId: number) {
  return execute("collect_agent_result", { agentId });
}

// --- Admin subprocess ---

export function createAdminSubprocess(
  workspaceRoots: string[],
  backendKind?: BackendKind,
) {
  return execute("create_admin_subprocess", { workspaceRoots, backendKind });
}

export function closeAdminSubprocess(adminId: number) {
  return execute("close_admin_subprocess", { adminId });
}

export function adminListSessions(adminId: number) {
  return execute("admin_list_sessions", { adminId });
}

export function adminGetSettings(adminId: number) {
  return execute("admin_get_settings", { adminId });
}

export function adminUpdateSettings(
  adminId: number,
  settings: Record<string, unknown>,
) {
  return execute("admin_update_settings", { adminId, settings });
}

export function adminListProfiles(adminId: number) {
  return execute("admin_list_profiles", { adminId });
}

export function adminSwitchProfile(adminId: number, profileName: string) {
  return execute("admin_switch_profile", { adminId, profileName });
}

export function adminGetModuleSchemas(adminId: number) {
  return execute("admin_get_module_schemas", { adminId });
}

export function adminDeleteSession(adminId: number, sessionId: string) {
  return execute("admin_delete_session", { adminId, sessionId });
}

// --- Git operations ---

export function discoverGitRepos(workspaceDir: string): Promise<string[]> {
  return invoke<string[]>("discover_git_repos", { workspaceDir }).catch(
    (err) => {
      console.error("bridge: discoverGitRepos failed:", err);
      throw new Error(friendlyError(String(err)));
    },
  );
}

export function gitCurrentBranch(workingDir: string) {
  return execute("git_current_branch", { workingDir });
}

export function gitStatus(workingDir: string) {
  return execute("git_status", { workingDir });
}

export function gitStage(workingDir: string, paths: string[]) {
  return execute("git_stage", { workingDir, paths });
}

export function gitUnstage(workingDir: string, paths: string[]) {
  return execute("git_unstage", { workingDir, paths });
}

export function gitCommit(workingDir: string, message: string) {
  return execute("git_commit", { workingDir, message });
}

export function gitDiff(workingDir: string, path: string, staged: boolean) {
  return execute("git_diff", { workingDir, path, staged });
}

export function gitDiffBaseContent(
  workingDir: string,
  path: string,
  staged: boolean,
) {
  return execute("git_diff_base_content", { workingDir, path, staged });
}

export function gitDiscard(workingDir: string, paths: string[]) {
  return execute("git_discard", { workingDir, paths });
}

export function gitWorktreeAdd(
  workingDir: string,
  path: string,
  branch: string,
) {
  return execute("git_worktree_add", { workingDir, path, branch });
}

export function gitWorktreeRemove(workingDir: string, path: string) {
  return execute("git_worktree_remove", { workingDir, path });
}

// --- File operations ---

export function listDirectory(path: string, showHidden = false) {
  return execute("list_directory", { path, showHidden });
}

export function readFileContent(path: string) {
  return execute("read_file_content", { path });
}

export function syncFileWatchPaths(paths: string[]) {
  return execute("sync_file_watch_paths", { paths });
}

export function watchWorkspaceDir(path: string) {
  return execute("watch_workspace_dir", { path });
}

export function unwatchWorkspaceDir() {
  return execute("unwatch_workspace_dir", {} as Record<string, never>);
}

// --- Terminal ---

export function createTerminal(workspacePath: string) {
  return execute("create_terminal", { workspacePath });
}

export function writeTerminal(terminalId: number, data: string) {
  return execute("write_terminal", { terminalId, data });
}

export function resizeTerminal(terminalId: number, cols: number, rows: number) {
  return execute("resize_terminal", { terminalId, cols, rows });
}

export function closeTerminal(terminalId: number) {
  return execute("close_terminal", { terminalId });
}

// --- MCP HTTP server ---

export function getMcpHttpServerSettings() {
  return execute("get_mcp_http_server_settings", {} as Record<string, never>);
}

export function setMcpHttpServerEnabled(enabled: boolean) {
  return execute("set_mcp_http_server_enabled", { enabled });
}

export function setMcpControlEnabled(enabled: boolean): Promise<void> {
  return invoke<void>("set_mcp_control_enabled", { enabled }).catch((err) => {
    console.error("bridge: setMcpControlEnabled failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export function getDriverMcpHttpServerSettings() {
  return execute(
    "get_driver_mcp_http_server_settings",
    {} as Record<string, never>,
  );
}

export function setDriverMcpHttpServerEnabled(enabled: boolean) {
  return execute("set_driver_mcp_http_server_enabled", { enabled });
}

export function setDriverMcpHttpServerAutoloadEnabled(enabled: boolean) {
  return execute("set_driver_mcp_http_server_autoload_enabled", { enabled });
}

export function setDefaultBackend(backend: string) {
  return execute("set_default_backend", { backend });
}

// --- Backend management ---

export function checkBackendDependencies() {
  return execute("check_backend_dependencies", {} as Record<string, never>);
}

export function setDisabledBackends(backends: string[]) {
  return execute("set_disabled_backends", { backends });
}

export function installBackendDependency(backendKind: string) {
  return execute("install_backend_dependency", { backendKind });
}

// --- Host management ---

export function listHosts(): Promise<Host[]> {
  return invoke<Host[]>("list_hosts").catch((err) => {
    console.error("bridge: listHosts failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export function addHost(label: string, hostname: string): Promise<Host> {
  return invoke<Host>("add_host", { label, hostname }).catch((err) => {
    console.error("bridge: addHost failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export function removeHost(id: string): Promise<void> {
  return invoke<void>("remove_host", { id }).catch((err) => {
    console.error("bridge: removeHost failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export function updateHostLabel(id: string, label: string): Promise<void> {
  return invoke<void>("update_host_label", { id, label }).catch((err) => {
    console.error("bridge: updateHostLabel failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export function updateHostEnabledBackends(
  id: string,
  backends: string[],
): Promise<void> {
  return invoke<void>("update_host_enabled_backends", { id, backends }).catch(
    (err) => {
      console.error("bridge: updateHostEnabledBackends failed:", err);
      throw new Error(friendlyError(String(err)));
    },
  );
}

export function updateHostDefaultBackend(
  id: string,
  backend: string,
): Promise<void> {
  return invoke<void>("update_host_default_backend", { id, backend }).catch(
    (err) => {
      console.error("bridge: updateHostDefaultBackend failed:", err);
      throw new Error(friendlyError(String(err)));
    },
  );
}

export function getHostForWorkspace(
  workspacePath: string,
): Promise<Host> {
  return invoke<Host>("get_host_for_workspace", { workspacePath }).catch(
    (err) => {
      console.error("bridge: getHostForWorkspace failed:", err);
      throw new Error(friendlyError(String(err)));
    },
  );
}

// --- Process management ---

export function restartSubprocess(conversationId: number) {
  return execute("restart_subprocess", { conversationId });
}

export function listActiveConversations() {
  return execute("list_active_conversations", {} as Record<string, never>);
}

export function shutdownAllSubprocesses() {
  return execute("shutdown_all_subprocesses", {} as Record<string, never>);
}

// --- Other ---

export function submitFeedback(feedback: string) {
  return execute("submit_feedback", { feedback });
}

export function submitDebugUiResponse(
  requestId: string,
  ok: boolean,
  result?: unknown,
  error?: string,
) {
  return execute("submit_debug_ui_response", { requestId, ok, result, error });
}

// --- Workbench events ---

export interface CreateWorkbenchEventPayload {
  parent_workspace_path: string;
  branch: string;
  worktree_path: string;
}

export function onCreateWorkbench(
  callback: (payload: CreateWorkbenchEventPayload) => void,
): Promise<UnlistenFn> {
  return listen<CreateWorkbenchEventPayload>(
    "tyde-create-workbench",
    (event) => {
      callback(event.payload);
    },
  );
}

export interface DeleteWorkbenchEventPayload {
  workspace_path: string;
}

export function onDeleteWorkbench(
  callback: (payload: DeleteWorkbenchEventPayload) => void,
): Promise<UnlistenFn> {
  return listen<DeleteWorkbenchEventPayload>(
    "tyde-delete-workbench",
    (event) => {
      callback(event.payload);
    },
  );
}

// --- Agent change events ---

export function onAgentChanged(
  callback: (agent: RuntimeAgent) => void,
): Promise<UnlistenFn> {
  return listen<RuntimeAgent>("agent-changed", (event) => {
    callback(event.payload);
  });
}

// --- Event listeners (Tauri-specific) ---

export function onChatEvent(
  onRegistered: (payload: ConversationRegisteredPayload) => void,
  onEvent: (payload: ChatEventPayload) => void,
): Promise<UnlistenFn> {
  return listen<{ conversation_id: number; event: unknown }>(
    "chat-event",
    (event) => {
      const raw = event.payload.event as { kind?: string; data?: unknown };
      if (raw.kind === "ConversationRegistered") {
        try {
          onRegistered({
            conversation_id: event.payload.conversation_id,
            data: raw.data as ConversationRegisteredData,
          });
        } catch (err) {
          console.error(
            "bridge: ConversationRegistered handler threw:",
            err,
            event.payload,
          );
        }
        return;
      }

      try {
        onEvent({
          conversation_id: event.payload.conversation_id,
          event: parseChatEvent(event.payload.event),
        });
      } catch (err) {
        if (err instanceof ProtocolParseError) {
          console.error(
            "bridge: invalid chat event payload:",
            err.message,
            err.payload,
          );
          return;
        }
        console.error("bridge: chat event handler threw:", err, event.payload);
      }
    },
  );
}

export function onAdminEvent(
  callback: (payload: AdminEventPayload) => void,
): Promise<UnlistenFn> {
  return listen<{ admin_id: number; event: unknown }>(
    "admin-event",
    (event) => {
      try {
        callback({
          admin_id: event.payload.admin_id,
          event: parseChatEvent(event.payload.event),
        });
      } catch (err) {
        if (err instanceof ProtocolParseError) {
          console.error(
            "bridge: invalid admin event payload:",
            err.message,
            err.payload,
          );
          return;
        }
        console.error("bridge: admin event handler threw:", err, event.payload);
      }
    },
  );
}

export function onFileChanged(
  callback: (payload: FileChangedPayload) => void,
): Promise<UnlistenFn> {
  return listen<FileChangedPayload>("file-changed", (event) => {
    callback(event.payload);
  });
}

export function onTerminalOutput(
  callback: (payload: TerminalOutputPayload) => void,
): Promise<UnlistenFn> {
  return listen<TerminalOutputPayload>("terminal-output", (event) => {
    callback(event.payload);
  });
}

export function onTerminalExit(
  callback: (payload: TerminalExitPayload) => void,
): Promise<UnlistenFn> {
  return listen<TerminalExitPayload>("terminal-exit", (event) => {
    callback(event.payload);
  });
}

export function onRemoteConnectionProgress(
  callback: (payload: RemoteConnectionProgress) => void,
): Promise<UnlistenFn> {
  return listen<RemoteConnectionProgress>(
    "remote-connection-progress",
    (event) => {
      callback(event.payload);
    },
  );
}

// --- Desktop-only utilities ---

const RECENT_WORKSPACES_KEY = "tyde-recent-workspaces";
const MAX_RECENT_WORKSPACES = 10;

export function getRecentWorkspaces(): string[] {
  const raw = localStorage.getItem(RECENT_WORKSPACES_KEY);
  if (!raw) return [];
  const parsed = JSON.parse(raw);
  if (!Array.isArray(parsed)) return [];
  return parsed
    .filter((s: unknown) => typeof s === "string")
    .slice(0, MAX_RECENT_WORKSPACES);
}

export function addRecentWorkspace(path: string): void {
  const recent = getRecentWorkspaces();
  const idx = recent.indexOf(path);
  if (idx !== -1) recent.splice(idx, 1);
  recent.unshift(path);
  if (recent.length > MAX_RECENT_WORKSPACES)
    recent.length = MAX_RECENT_WORKSPACES;
  try {
    localStorage.setItem(RECENT_WORKSPACES_KEY, JSON.stringify(recent));
  } catch (err) {
    console.error("Failed to save recent workspaces to localStorage:", err);
  }
}

export interface BackendUsageWindow {
  id: string;
  label: string;
  used_percent: number | null;
  reset_at_text: string | null;
  reset_at_unix: number | null;
  window_minutes: number | null;
}

export interface BackendUsageResult {
  backend_kind: BackendKind;
  source: string;
  captured_at_ms: number;
  plan: string | null;
  status: string | null;
  windows: BackendUsageWindow[];
  details: string[];
}

export function queryBackendUsage(
  backendKind: BackendKind,
  hostId?: string,
): Promise<BackendUsageResult> {
  return invoke<BackendUsageResult>("query_backend_usage", {
    backendKind,
    hostId,
  }).catch((err) => {
    console.error("bridge: queryBackendUsage failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function openWorkspaceDialog(): Promise<string | null> {
  try {
    const selected = await open({
      directory: true,
      multiple: false,
      title: "Select Workspace Directory",
    });
    if (typeof selected === "string") return selected;
    return null;
  } catch (err) {
    console.error("bridge: openWorkspaceDialog failed:", err);
    throw new Error(friendlyError(String(err)));
  }
}

export async function getInitialWorkspace(): Promise<string | null> {
  return invoke<string | null>("get_initial_workspace");
}

// --- Workflow operations ---

export function listWorkflows(workspacePath?: string) {
  return execute("list_workflows", { workspacePath });
}

export function saveWorkflow(
  workflowJson: string,
  scope: string,
  workspacePath?: string,
) {
  return execute("save_workflow", { workflowJson, scope, workspacePath });
}

export function deleteWorkflow(
  id: string,
  scope: string,
  workspacePath?: string,
) {
  return execute("delete_workflow", { id, scope, workspacePath });
}

export function runShellCommand(command: string, cwd: string) {
  return execute("run_shell_command", { command, cwd });
}
