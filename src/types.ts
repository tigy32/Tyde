import type {
  ChatMessage,
  ContextBreakdown,
  ChatEvent as CoreChatEvent,
  ChatEventTag as CoreChatEventTag,
  SessionMetadata as CoreSessionMetadata,
  ImageData,
  MessageSender,
  Model,
  ModuleSchemaInfo,
  Task,
  TaskList,
  TokenUsage,
  ToolExecutionResult,
  ToolRequest,
  ToolRequestType,
} from "./core_types";
import type { GeneratedChatEventKind } from "./generated/protocol_kinds";

type AssertTrue<T extends true> = T;
type Missing<Expected extends string, Actual extends string> = Exclude<
  Expected,
  Actual
>;

type _CoreEventKindsAreUpToDate = AssertTrue<
  Missing<GeneratedChatEventKind, CoreChatEventTag> extends never ? true : false
>;

export type {
  ChatMessage,
  ContextBreakdown,
  ImageData,
  MessageSender,
  ModuleSchemaInfo,
  Task,
  TaskList,
  TokenUsage,
  ToolExecutionResult,
  ToolRequest,
  ToolRequestType,
};

export interface TaskItem {
  id?: number;
  description: string;
  status: Task["status"];
}

export interface SessionMetadata extends CoreSessionMetadata {
  session_id?: string;
  created_at?: number;
  message_count?: number;
  last_message_preview?: string;
  preview?: string;
  workspace_root?: string;
  backend_kind?: string;
}

export type ModelInfo = Model;

export interface ImageAttachment {
  data: string;
  media_type: string;
  name: string;
  size: number;
}

export type ChatEvent =
  | CoreChatEvent
  | { kind: "SubprocessStderr"; data: string }
  | { kind: "SubprocessExit"; data: { exit_code: number | null } };

export type ChatEventTag = ChatEvent["kind"];

export interface GitFileStatus {
  path: string;
  status:
    | "Modified"
    | "Added"
    | "Deleted"
    | "Renamed"
    | "Untracked"
    | "Conflicted";
  staged: boolean;
}

export interface FileEntry {
  name: string;
  path: string;
  is_directory: boolean;
  size: number | null;
}

export interface FileContent {
  path: string;
  content: string;
  size: number;
  truncated: boolean;
}
