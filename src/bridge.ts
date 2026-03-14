import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { ProtocolParseError, parseChatEvent } from "./protocol";
import type {
  ChatEvent,
  FileContent,
  FileEntry,
  GitFileStatus,
  ImageAttachment,
} from "./types";

function friendlyError(raw: string): string {
  const msg = String(raw);
  return msg.length > 200 ? `${msg.slice(0, 200)}…` : msg;
}

export interface ChatEventPayload {
  conversation_id: number;
  event: ChatEvent;
}

export interface ConversationRegisteredData {
  agent_id: number | null;
  workspace_roots: string[];
  backend_kind: string;
  name: string;
  agent_type: string | null;
  parent_agent_id: number | null;
}

export interface ConversationRegisteredPayload {
  conversation_id: number;
  data: ConversationRegisteredData;
}

export type BackendKind = "tycode" | "codex" | "claude" | "kiro";

export type RuntimeAgentStatus =
  | "queued"
  | "running"
  | "waiting_input"
  | "completed"
  | "failed"
  | "cancelled";

export interface RuntimeAgent {
  agent_id: number;
  conversation_id: number;
  workspace_roots: string[];
  backend_kind: string;
  parent_agent_id: number | null;
  name: string;
  agent_type: string | null;
  status: RuntimeAgentStatus;
  summary: string;
  created_at_ms: number;
  updated_at_ms: number;
  ended_at_ms: number | null;
  last_error: string | null;
  last_message: string | null;
}

export interface RuntimeAgentEvent {
  seq: number;
  agent_id: number;
  conversation_id: number;
  kind: string;
  status: RuntimeAgentStatus;
  timestamp_ms: number;
  message: string | null;
}

export interface RuntimeAgentEventBatch {
  events: RuntimeAgentEvent[];
  latest_seq: number;
}

export interface SpawnAgentResponse {
  agent_id: number;
  conversation_id: number;
}

export interface CollectedAgentResult {
  agent: RuntimeAgent;
  final_message: string | null;
  changed_files: string[];
  tool_results: unknown[];
}

export interface McpHttpServerSettings {
  enabled: boolean;
  running: boolean;
  url: string | null;
}

export interface DebugMcpHttpServerSettings {
  enabled: boolean;
  autoload: boolean;
  running: boolean;
  url: string | null;
}

export type ConversationMode = "standard" | "bridge";

export async function createConversation(
  workspaceRoots: string[],
  backendKind?: BackendKind,
  ephemeral?: boolean,
  conversationMode?: ConversationMode,
): Promise<number> {
  return invoke<number>("create_conversation", {
    workspaceRoots,
    backendKind,
    ephemeral,
    conversationMode,
  }).catch((err) => {
    console.error("bridge: createConversation failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function sendMessage(
  conversationId: number,
  message: string,
  images?: ImageAttachment[],
): Promise<void> {
  return invoke<void>("send_message", {
    conversationId,
    message,
    images,
  }).catch((err) => {
    console.error("bridge: sendMessage failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function cancelConversation(
  conversationId: number,
): Promise<void> {
  return invoke<void>("cancel_conversation", { conversationId }).catch(
    (err) => {
      console.error("bridge: cancelConversation failed:", err);
      throw new Error(friendlyError(String(err)));
    },
  );
}

export async function closeConversation(conversationId: number): Promise<void> {
  return invoke<void>("close_conversation", { conversationId }).catch((err) => {
    console.error("bridge: closeConversation failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function getSettings(conversationId: number): Promise<void> {
  return invoke<void>("get_settings", { conversationId }).catch((err) => {
    console.error("bridge: getSettings failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function listSessions(conversationId: number): Promise<void> {
  return invoke<void>("list_sessions", { conversationId }).catch((err) => {
    console.error("bridge: listSessions failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function resumeSession(
  conversationId: number,
  sessionId: string,
): Promise<void> {
  return invoke<void>("resume_session", { conversationId, sessionId }).catch(
    (err) => {
      console.error("bridge: resumeSession failed:", err);
      throw new Error(friendlyError(String(err)));
    },
  );
}

export async function getSessionId(
  conversationId: number,
): Promise<string | null> {
  return invoke<string | null>("get_session_id", { conversationId }).catch(
    (err) => {
      console.error("bridge: getSessionId failed:", err);
      throw new Error(friendlyError(String(err)));
    },
  );
}

export async function deleteSession(
  conversationId: number,
  sessionId: string,
): Promise<void> {
  return invoke<void>("delete_session", { conversationId, sessionId }).catch(
    (err) => {
      console.error("bridge: deleteSession failed:", err);
      throw new Error(friendlyError(String(err)));
    },
  );
}

export async function exportSessionJson(sessionId: string): Promise<string> {
  return invoke<string>("export_session_json", { sessionId }).catch((err) => {
    console.error("bridge: exportSessionJson failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function listModels(conversationId: number): Promise<void> {
  return invoke<void>("list_models", { conversationId }).catch((err) => {
    console.error("bridge: listModels failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function listProfiles(conversationId: number): Promise<void> {
  return invoke<void>("list_profiles", { conversationId }).catch((err) => {
    console.error("bridge: listProfiles failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function switchProfile(
  conversationId: number,
  profileName: string,
): Promise<void> {
  return invoke<void>("switch_profile", { conversationId, profileName }).catch(
    (err) => {
      console.error("bridge: switchProfile failed:", err);
      throw new Error(friendlyError(String(err)));
    },
  );
}

export async function getModuleSchemas(conversationId: number): Promise<void> {
  return invoke<void>("get_module_schemas", { conversationId }).catch((err) => {
    console.error("bridge: getModuleSchemas failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function updateSettings(
  conversationId: number,
  settings: Record<string, unknown>,
): Promise<void> {
  return invoke<void>("update_settings", { conversationId, settings }).catch(
    (err) => {
      console.error("bridge: updateSettings failed:", err);
      throw new Error(friendlyError(String(err)));
    },
  );
}

export function onChatEvent(
  onRegistered: (payload: ConversationRegisteredPayload) => void,
  onEvent: (payload: ChatEventPayload) => void,
): Promise<UnlistenFn> {
  return listen<{ conversation_id: number; event: unknown }>(
    "chat-event",
    (event) => {
      const raw = event.payload.event as { kind?: string; data?: unknown };
      if (raw.kind === "ConversationRegistered") {
        onRegistered({
          conversation_id: event.payload.conversation_id,
          data: raw.data as ConversationRegisteredData,
        });
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
        throw err;
      }
    },
  );
}

export async function gitCurrentBranch(workingDir: string): Promise<string> {
  return invoke<string>("git_current_branch", { workingDir }).catch((err) => {
    console.error("bridge: gitCurrentBranch failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function gitStatus(workingDir: string): Promise<GitFileStatus[]> {
  return invoke<GitFileStatus[]>("git_status", { workingDir }).catch((err) => {
    console.error("bridge: gitStatus failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function gitStage(
  workingDir: string,
  paths: string[],
): Promise<void> {
  return invoke<void>("git_stage", { workingDir, paths }).catch((err) => {
    console.error("bridge: gitStage failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function gitUnstage(
  workingDir: string,
  paths: string[],
): Promise<void> {
  return invoke<void>("git_unstage", { workingDir, paths }).catch((err) => {
    console.error("bridge: gitUnstage failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function gitCommit(
  workingDir: string,
  message: string,
): Promise<string> {
  return invoke<string>("git_commit", { workingDir, message }).catch((err) => {
    console.error("bridge: gitCommit failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function gitDiff(
  workingDir: string,
  path: string,
  staged: boolean,
): Promise<string> {
  return invoke<string>("git_diff", { workingDir, path, staged }).catch(
    (err) => {
      console.error("bridge: gitDiff failed:", err);
      throw new Error(friendlyError(String(err)));
    },
  );
}

export async function gitDiffBaseContent(
  workingDir: string,
  path: string,
  staged: boolean,
): Promise<string> {
  return invoke<string>("git_diff_base_content", {
    workingDir,
    path,
    staged,
  }).catch((err) => {
    console.error("bridge: gitDiffBaseContent failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function gitDiscard(
  workingDir: string,
  paths: string[],
): Promise<void> {
  return invoke<void>("git_discard", { workingDir, paths }).catch((err) => {
    console.error("bridge: gitDiscard failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function gitWorktreeAdd(
  workingDir: string,
  path: string,
  branch: string,
): Promise<void> {
  return invoke<void>("git_worktree_add", { workingDir, path, branch }).catch(
    (err) => {
      console.error("bridge: gitWorktreeAdd failed:", err);
      throw new Error(friendlyError(String(err)));
    },
  );
}

export async function gitWorktreeRemove(
  workingDir: string,
  path: string,
): Promise<void> {
  return invoke<void>("git_worktree_remove", { workingDir, path }).catch(
    (err) => {
      console.error("bridge: gitWorktreeRemove failed:", err);
      throw new Error(friendlyError(String(err)));
    },
  );
}

export async function listDirectory(
  path: string,
  showHidden: boolean = false,
): Promise<FileEntry[]> {
  return invoke<FileEntry[]>("list_directory", { path, showHidden }).catch(
    (err) => {
      console.error("bridge: listDirectory failed:", err);
      throw new Error(friendlyError(String(err)));
    },
  );
}

export async function readFileContent(path: string): Promise<FileContent> {
  return invoke<FileContent>("read_file_content", { path }).catch((err) => {
    console.error("bridge: readFileContent failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function syncFileWatchPaths(paths: string[]): Promise<void> {
  return invoke<void>("sync_file_watch_paths", { paths }).catch((err) => {
    console.error("bridge: syncFileWatchPaths failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function watchWorkspaceDir(path: string): Promise<void> {
  return invoke<void>("watch_workspace_dir", { path }).catch((err) => {
    console.error("bridge: watchWorkspaceDir failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function unwatchWorkspaceDir(): Promise<void> {
  return invoke<void>("unwatch_workspace_dir").catch((err) => {
    console.error("bridge: unwatchWorkspaceDir failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export interface FileChangedPayload {
  path: string;
}

export function onFileChanged(
  callback: (payload: FileChangedPayload) => void,
): Promise<UnlistenFn> {
  return listen<FileChangedPayload>("file-changed", (event) => {
    callback(event.payload);
  });
}

export async function createTerminal(workspacePath: string): Promise<number> {
  return invoke<number>("create_terminal", { workspacePath }).catch((err) => {
    console.error("bridge: createTerminal failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function writeTerminal(
  terminalId: number,
  data: string,
): Promise<void> {
  return invoke<void>("write_terminal", { terminalId, data }).catch((err) => {
    console.error("bridge: writeTerminal failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function resizeTerminal(
  terminalId: number,
  cols: number,
  rows: number,
): Promise<void> {
  return invoke<void>("resize_terminal", { terminalId, cols, rows }).catch(
    (err) => {
      console.error("bridge: resizeTerminal failed:", err);
      throw new Error(friendlyError(String(err)));
    },
  );
}

export async function closeTerminal(terminalId: number): Promise<void> {
  return invoke<void>("close_terminal", { terminalId }).catch((err) => {
    console.error("bridge: closeTerminal failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function restartSubprocess(conversationId: number): Promise<void> {
  return invoke<void>("restart_subprocess", { conversationId }).catch((err) => {
    console.error("bridge: restartSubprocess failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

// --- Admin subprocess bridge functions ---

export interface AdminEventPayload {
  admin_id: number;
  event: ChatEvent;
}

export async function createAdminSubprocess(
  workspaceRoots: string[],
  backendKind?: BackendKind,
): Promise<number> {
  return invoke<number>("create_admin_subprocess", {
    workspaceRoots,
    backendKind,
  }).catch((err) => {
    console.error("bridge: createAdminSubprocess failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function closeAdminSubprocess(adminId: number): Promise<void> {
  return invoke<void>("close_admin_subprocess", { adminId }).catch((err) => {
    console.error("bridge: closeAdminSubprocess failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function adminListSessions(adminId: number): Promise<void> {
  return invoke<void>("admin_list_sessions", { adminId }).catch((err) => {
    console.error("bridge: adminListSessions failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function adminGetSettings(adminId: number): Promise<void> {
  return invoke<void>("admin_get_settings", { adminId }).catch((err) => {
    console.error("bridge: adminGetSettings failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function adminUpdateSettings(
  adminId: number,
  settings: Record<string, unknown>,
): Promise<void> {
  return invoke<void>("admin_update_settings", { adminId, settings }).catch(
    (err) => {
      console.error("bridge: adminUpdateSettings failed:", err);
      throw new Error(friendlyError(String(err)));
    },
  );
}

export async function adminListProfiles(adminId: number): Promise<void> {
  return invoke<void>("admin_list_profiles", { adminId }).catch((err) => {
    console.error("bridge: adminListProfiles failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function adminSwitchProfile(
  adminId: number,
  profileName: string,
): Promise<void> {
  return invoke<void>("admin_switch_profile", { adminId, profileName }).catch(
    (err) => {
      console.error("bridge: adminSwitchProfile failed:", err);
      throw new Error(friendlyError(String(err)));
    },
  );
}

export async function adminGetModuleSchemas(adminId: number): Promise<void> {
  return invoke<void>("admin_get_module_schemas", { adminId }).catch((err) => {
    console.error("bridge: adminGetModuleSchemas failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function adminDeleteSession(
  adminId: number,
  sessionId: string,
): Promise<void> {
  return invoke<void>("admin_delete_session", { adminId, sessionId }).catch(
    (err) => {
      console.error("bridge: adminDeleteSession failed:", err);
      throw new Error(friendlyError(String(err)));
    },
  );
}

export async function getMcpHttpServerSettings(): Promise<McpHttpServerSettings> {
  return invoke<McpHttpServerSettings>("get_mcp_http_server_settings").catch(
    (err) => {
      console.error("bridge: getMcpHttpServerSettings failed:", err);
      throw new Error(friendlyError(String(err)));
    },
  );
}

export async function setMcpHttpServerEnabled(
  enabled: boolean,
): Promise<McpHttpServerSettings> {
  return invoke<McpHttpServerSettings>("set_mcp_http_server_enabled", {
    enabled,
  }).catch((err) => {
    console.error("bridge: setMcpHttpServerEnabled failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function getDebugMcpHttpServerSettings(): Promise<DebugMcpHttpServerSettings> {
  return invoke<DebugMcpHttpServerSettings>(
    "get_debug_mcp_http_server_settings",
  ).catch((err) => {
    console.error("bridge: getDebugMcpHttpServerSettings failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function setDebugMcpHttpServerEnabled(
  enabled: boolean,
): Promise<DebugMcpHttpServerSettings> {
  return invoke<DebugMcpHttpServerSettings>(
    "set_debug_mcp_http_server_enabled",
    { enabled },
  ).catch((err) => {
    console.error("bridge: setDebugMcpHttpServerEnabled failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function setDebugMcpHttpServerAutoloadEnabled(
  enabled: boolean,
): Promise<DebugMcpHttpServerSettings> {
  return invoke<DebugMcpHttpServerSettings>(
    "set_debug_mcp_http_server_autoload_enabled",
    { enabled },
  ).catch((err) => {
    console.error("bridge: setDebugMcpHttpServerAutoloadEnabled failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function submitDebugUiResponse(
  requestId: string,
  ok: boolean,
  result?: unknown,
  error?: string,
): Promise<void> {
  return invoke<void>("submit_debug_ui_response", {
    requestId,
    ok,
    result,
    error,
  }).catch((err) => {
    console.error("bridge: submitDebugUiResponse failed:", err);
    throw new Error(friendlyError(String(err)));
  });
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
        throw err;
      }
    },
  );
}

export async function listActiveConversations(): Promise<number[]> {
  return invoke<number[]>("list_active_conversations").catch((err) => {
    console.error("bridge: listActiveConversations failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function spawnAgent(
  workspaceRoots: string[],
  prompt: string,
  backendKind?: BackendKind,
  parentAgentId?: number,
  name?: string,
  ephemeral?: boolean,
): Promise<SpawnAgentResponse> {
  return invoke<SpawnAgentResponse>("spawn_agent", {
    workspaceRoots,
    prompt,
    backendKind,
    parentAgentId,
    name,
    ephemeral,
  }).catch((err) => {
    console.error("bridge: spawnAgent failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function sendAgentMessage(
  agentId: number,
  message: string,
): Promise<void> {
  return invoke<void>("send_agent_message", { agentId, message }).catch(
    (err) => {
      console.error("bridge: sendAgentMessage failed:", err);
      throw new Error(friendlyError(String(err)));
    },
  );
}

export async function interruptAgent(agentId: number): Promise<void> {
  return invoke<void>("interrupt_agent", { agentId }).catch((err) => {
    console.error("bridge: interruptAgent failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function terminateAgent(agentId: number): Promise<void> {
  return invoke<void>("terminate_agent", { agentId }).catch((err) => {
    console.error("bridge: terminateAgent failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function getAgent(agentId: number): Promise<RuntimeAgent | null> {
  return invoke<RuntimeAgent | null>("get_agent", { agentId }).catch((err) => {
    console.error("bridge: getAgent failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function listAgents(): Promise<RuntimeAgent[]> {
  return invoke<RuntimeAgent[]>("list_agents").catch((err) => {
    console.error("bridge: listAgents failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function waitForAgent(
  agentId: number,
  until?: "idle" | "completed" | "failed" | "needs_input" | "terminal",
  timeoutMs?: number,
): Promise<RuntimeAgent> {
  return invoke<RuntimeAgent>("wait_for_agent", {
    agentId,
    until,
    timeoutMs,
  }).catch((err) => {
    console.error("bridge: waitForAgent failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function agentEventsSince(
  sinceSeq: number = 0,
  limit: number = 200,
): Promise<RuntimeAgentEventBatch> {
  return invoke<RuntimeAgentEventBatch>("agent_events_since", {
    sinceSeq,
    limit,
  }).catch((err) => {
    console.error("bridge: agentEventsSince failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

export async function collectAgentResult(
  agentId: number,
): Promise<CollectedAgentResult> {
  return invoke<CollectedAgentResult>("collect_agent_result", {
    agentId,
  }).catch((err) => {
    console.error("bridge: collectAgentResult failed:", err);
    throw new Error(friendlyError(String(err)));
  });
}

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

export interface RemoteConnectionProgress {
  host: string;
  step: string;
  status: string;
  message: string;
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

export interface TerminalOutputPayload {
  terminal_id: number;
  data: string;
}

export interface TerminalExitPayload {
  terminal_id: number;
  exit_code: number | null;
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
