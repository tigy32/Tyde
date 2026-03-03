// Canonical TypeScript protocol types, inlined from tycode-client-typescript.
// Discriminant unions are generated from Rust enums (see generated/protocol_kinds.ts).

import type {
  GeneratedChatActorMessageVariant,
  GeneratedChatEventKind,
  GeneratedModelVariant,
} from "./generated/protocol_kinds";

export {
  GENERATED_CHAT_ACTOR_MESSAGE_VARIANTS,
  GENERATED_CHAT_EVENT_KINDS,
  GENERATED_MODEL_VARIANTS,
} from "./generated/protocol_kinds";

type AssertTrue<T extends true> = T;
type Missing<Expected extends string, Actual extends string> = Exclude<
  Expected,
  Actual
>;
type Extra<Expected extends string, Actual extends string> = Exclude<
  Actual,
  Expected
>;

export interface ModuleSchemaInfo {
  namespace: string;
  schema: object;
}

export interface DurationLike {
  secs: number;
  nanos: number;
}

export interface ContextBreakdown {
  system_prompt_bytes: number;
  tool_io_bytes: number;
  conversation_history_bytes: number;
  reasoning_bytes: number;
  context_injection_bytes: number;
  input_tokens: number;
  context_window: number;
}

export interface FileInfo {
  path: string;
  bytes: number;
}

export interface ContextInfo {
  directory_list_bytes: number;
  files: FileInfo[];
}

export { GENERATED_MODEL_VARIANTS as MODEL_VARIANTS } from "./generated/protocol_kinds";

export type Model = GeneratedModelVariant;

export interface ModelInfo {
  model: Model;
}

export type MessageSender =
  | "User"
  | "System"
  | "Warning"
  | "Error"
  | { Assistant: { agent: string } };

export interface ReasoningData {
  text: string;
  tokens?: number;
  signature?: string;
  blob?: number[];
}

export interface ToolUseData {
  id: string;
  name: string;
  arguments: unknown;
}

export interface TokenUsage {
  input_tokens: number;
  output_tokens: number;
  total_tokens: number;
  cached_prompt_tokens?: number;
  cache_creation_input_tokens?: number;
  reasoning_tokens?: number;
}

export interface ImageData {
  media_type: string;
  data: string;
}

export interface ChatMessage {
  timestamp: number;
  sender: MessageSender;
  content: string;
  reasoning?: ReasoningData | null;
  tool_calls: ToolUseData[];
  model_info?: ModelInfo | null;
  token_usage?: TokenUsage | null;
  context_breakdown?: ContextBreakdown | null;
  images?: ImageData[];
}

export type ToolRequestType =
  | { kind: "ModifyFile"; file_path: string; before: string; after: string }
  | { kind: "RunCommand"; command: string; working_directory: string }
  | { kind: "ReadFiles"; file_paths: string[] }
  | {
      kind: "SearchTypes";
      language: string;
      workspace_root: string;
      type_name: string;
    }
  | {
      kind: "GetTypeDocs";
      language: string;
      workspace_root: string;
      type_path: string;
    }
  | { kind: "Other"; args: unknown };

export type ToolExecutionResult =
  | { kind: "ModifyFile"; lines_added: number; lines_removed: number }
  | { kind: "RunCommand"; exit_code: number; stdout: string; stderr: string }
  | { kind: "ReadFiles"; files: FileInfo[] }
  | { kind: "SearchTypes"; types: string[] }
  | { kind: "GetTypeDocs"; documentation: string }
  | { kind: "Error"; short_message: string; detailed_message: string }
  | { kind: "Other"; result: unknown };

export interface ToolRequest {
  tool_call_id: string;
  tool_name: string;
  tool_type: ToolRequestType;
}

export type TaskStatus = "pending" | "in_progress" | "completed" | "failed";

export interface Task {
  id: number;
  description: string;
  status: TaskStatus;
}

export interface TaskList {
  title: string;
  tasks: Task[];
}

export interface SessionMetadata {
  id: string;
  title: string;
  last_modified: number;
  session_id?: string;
  created_at?: number;
  message_count?: number;
  last_message_preview?: string;
  preview?: string;
  workspace_root?: string;
  backend_kind?: string;
}

export interface SessionData {
  id: string;
  created_at: number;
  last_modified: number;
  messages: unknown[];
  tracked_files: string[];
  events: ChatEvent[];
  [key: string]: unknown;
}

export interface ChatEventDataByKind {
  MessageAdded: ChatMessage;
  StreamStart: { message_id: string; agent: string; model: Model };
  StreamDelta: { message_id: string; text: string };
  StreamReasoningDelta: { message_id: string; text: string };
  StreamEnd: { message: ChatMessage };
  Settings: unknown;
  TypingStatusChanged: boolean;
  ConversationCleared: undefined;
  ToolRequest: ToolRequest;
  ToolExecutionCompleted: {
    tool_call_id: string;
    tool_name: string;
    tool_result: ToolExecutionResult;
    success: boolean;
    error?: string;
  };
  OperationCancelled: { message: string };
  RetryAttempt: {
    attempt: number;
    max_retries: number;
    error: string;
    backoff_ms: number;
  };
  TaskUpdate: TaskList;
  SessionsList: { sessions: SessionMetadata[] };
  ProfilesList: { profiles: string[] };
  ModelsList: {
    models: Array<{ id: string; displayName: string; isDefault: boolean }>;
  };
  TimingUpdate: {
    waiting_for_human: DurationLike;
    ai_processing: DurationLike;
    tool_execution: DurationLike;
  };
  ModuleSchemas: { schemas: ModuleSchemaInfo[] };
  Error: string;
}

type _ChatEventKindCoverage = AssertTrue<
  Missing<
    GeneratedChatEventKind,
    keyof ChatEventDataByKind & string
  > extends never
    ? true
    : false
>;
type _ChatEventKindNoExtra = AssertTrue<
  Extra<
    GeneratedChatEventKind,
    keyof ChatEventDataByKind & string
  > extends never
    ? true
    : false
>;

type EventOfKind<K extends GeneratedChatEventKind> =
  ChatEventDataByKind[K] extends undefined
    ? { kind: K }
    : { kind: K; data: ChatEventDataByKind[K] };

export type ChatEvent = {
  [K in GeneratedChatEventKind]: EventOfKind<K>;
}[GeneratedChatEventKind];

export type ChatEventTag = ChatEvent["kind"];

export function getChatEventTag(event: ChatEvent): ChatEventTag {
  return event.kind;
}

export interface ChatActorMessagePayloadByVariant {
  UserInput: string;
  UserInputWithImages: { text: string; images: ImageData[] };
  ChangeProvider: string;
  GetSettings: undefined;
  SaveSettings: { settings: unknown; persist: boolean };
  SwitchProfile: { profile_name: string };
  SaveProfile: { profile_name: string };
  ListProfiles: undefined;
  ListSessions: undefined;
  ResumeSession: { session_id: string };
  DeleteSession: { session_id: string };
  GetModuleSchemas: undefined;
}

type _ChatActorVariantCoverage = AssertTrue<
  Missing<
    GeneratedChatActorMessageVariant,
    keyof ChatActorMessagePayloadByVariant & string
  > extends never
    ? true
    : false
>;
type _ChatActorVariantNoExtra = AssertTrue<
  Extra<
    GeneratedChatActorMessageVariant,
    keyof ChatActorMessagePayloadByVariant & string
  > extends never
    ? true
    : false
>;

type ChatActorMessageOfVariant<K extends GeneratedChatActorMessageVariant> =
  ChatActorMessagePayloadByVariant[K] extends undefined
    ? K
    : { [P in K]: ChatActorMessagePayloadByVariant[K] };

export type ChatActorMessage = {
  [K in GeneratedChatActorMessageVariant]: ChatActorMessageOfVariant<K>;
}[GeneratedChatActorMessageVariant];
