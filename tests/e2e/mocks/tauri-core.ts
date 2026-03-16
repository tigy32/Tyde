import { emit } from '@tauri-apps/api/event';

interface ConversationState {
  workspaceRoots: string[];
  turnCount: number;
  backendKind: string;
}

interface AdminState {
  workspaceRoots: string[];
  backendKind: string;
}

let nextConversationId = 10_000;
const conversations = new Map<number, ConversationState>();

let nextAdminId = 50_000;
const adminSubprocesses = new Map<number, AdminState>();

type MockRuntimeAgentStatus =
  | 'queued'
  | 'running'
  | 'waiting_input'
  | 'completed'
  | 'failed'
  | 'cancelled';

interface MockRuntimeAgent {
  agent_id: number;
  conversation_id: number;
  workspace_roots: string[];
  backend_kind: string;
  parent_agent_id: number | null;
  keep_alive_without_tab: boolean;
  name: string;
  status: MockRuntimeAgentStatus;
  summary: string;
  created_at_ms: number;
  updated_at_ms: number;
  ended_at_ms: number | null;
  last_error: string | null;
  last_message: string | null;
}

interface MockRuntimeAgentEvent {
  seq: number;
  agent_id: number;
  conversation_id: number;
  kind: string;
  status: MockRuntimeAgentStatus;
  timestamp_ms: number;
  message: string | null;
}

let nextRuntimeAgentId = 1;
let nextRuntimeEventSeq = 1;
const runtimeAgents = new Map<number, MockRuntimeAgent>();
const runtimeEvents: MockRuntimeAgentEvent[] = [];

interface MockSessionState {
  id: string;
  title: string;
  created_at: number;
  last_modified: number;
  message_count: number;
  last_message_preview: string;
  workspace_root: string;
  backend_kind: string;
}

function defaultMockSessionsByBackend(): Record<string, MockSessionState[]> {
  const now = Date.now();
  return {
    tycode: [
      {
        id: 'tycode-session-1',
        title: 'Tycode Session 1',
        created_at: now - 30_000,
        last_modified: now - 10_000,
        message_count: 5,
        last_message_preview: 'Tycode Session 1',
        workspace_root: '/mock/workspace',
        backend_kind: 'tycode',
      },
    ],
    codex: [
      {
        id: 'codex-session-1',
        title: 'Codex Session 1',
        created_at: now - 25_000,
        last_modified: now - 8_000,
        message_count: 3,
        last_message_preview: 'Codex Session 1',
        workspace_root: '/mock/workspace',
        backend_kind: 'codex',
      },
    ],
    claude: [],
  };
}

let mockSessionsByBackend: Record<string, MockSessionState[]> = defaultMockSessionsByBackend();

let nextTerminalId = 70_000;
const terminalWorkspaceById = new Map<number, string>();
let mockMcpHttpServerEnabled = true;
const mockMcpHttpServerUrl = 'http://127.0.0.1:47771/mcp';
let mockDriverMcpHttpServerEnabled = false;
let mockDriverMcpHttpServerAutoload = false;
const mockDriverMcpHttpServerUrl = 'http://127.0.0.1:47772/mcp';

function syncMockMcpSettingsFromStorage(): void {
  try {
    const raw = window.localStorage.getItem('mock-mcp-http-enabled');
    if (raw === 'true') mockMcpHttpServerEnabled = true;
    if (raw === 'false') mockMcpHttpServerEnabled = false;
  } catch {
    // Ignore storage access failures in tests.
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}

function isTerminalRuntimeStatus(status: MockRuntimeAgentStatus): boolean {
  return status === 'completed' || status === 'failed' || status === 'cancelled';
}

function pushRuntimeEvent(agent: MockRuntimeAgent, kind: string, message: string | null): void {
  runtimeEvents.push({
    seq: nextRuntimeEventSeq++,
    agent_id: agent.agent_id,
    conversation_id: agent.conversation_id,
    kind,
    status: agent.status,
    timestamp_ms: Date.now(),
    message,
  });
  if (runtimeEvents.length > 5000) {
    runtimeEvents.splice(0, runtimeEvents.length - 5000);
  }
}

function updateRuntimeAgent(
  agentId: number,
  status: MockRuntimeAgentStatus,
  summary: string,
  kind: string,
  options?: { lastMessage?: string | null; lastError?: string | null },
): void {
  const agent = runtimeAgents.get(agentId);
  if (!agent) return;
  agent.status = status;
  agent.summary = summary;
  agent.updated_at_ms = Date.now();
  agent.last_message = options?.lastMessage ?? agent.last_message;
  agent.last_error = options?.lastError ?? (status === 'failed' ? agent.last_error : null);
  agent.ended_at_ms = isTerminalRuntimeStatus(status) ? Date.now() : null;
  pushRuntimeEvent(agent, kind, summary);
}

function sortedRuntimeAgents(): MockRuntimeAgent[] {
  return Array.from(runtimeAgents.values())
    .sort((a, b) => b.updated_at_ms - a.updated_at_ms)
    .map((agent) => ({ ...agent, workspace_roots: [...agent.workspace_roots] }));
}

async function emitRemoteProgress(host: string, failAtStep?: string): Promise<boolean> {
  const steps = [
    { step: 'validating_connection', inMsg: 'Testing SSH connection...', doneMsg: 'SSH connection established' },
    { step: 'checking_environment', inMsg: 'Checking remote environment...', doneMsg: 'tycode-subprocess not installed' },
    { step: 'installing_subprocess', inMsg: 'Building tycode-subprocess...', doneMsg: 'tycode-subprocess installed' },
    { step: 'ready', inMsg: 'Finalizing...', doneMsg: `Connected to ${host}` },
  ];

  for (const s of steps) {
    if (failAtStep === s.step) {
      await emit('remote-connection-progress', { host, step: s.step, status: 'failed', message: `Failed at ${s.step}` });
      return false;
    }
    await emit('remote-connection-progress', { host, step: s.step, status: 'in_progress', message: s.inMsg });
    await sleep(15);
    await emit('remote-connection-progress', { host, step: s.step, status: 'completed', message: s.doneMsg });
    await sleep(15);
  }
  return true;
}

async function emitChatEvent(conversationId: number, event: unknown): Promise<void> {
  await emit('chat-event', {
    conversation_id: conversationId,
    event,
  });
}

async function emitAdminEvent(adminId: number, event: unknown): Promise<void> {
  await emit('admin-event', {
    admin_id: adminId,
    event,
  });
}

function makeContextBreakdown(inputTokens: number): Record<string, number> {
  return {
    context_window: 200_000,
    input_tokens: inputTokens,
    system_prompt_bytes: 5_000,
    tool_io_bytes: 3_000,
    conversation_history_bytes: Math.max(12_000, inputTokens * 2),
    reasoning_bytes: 0,
    context_injection_bytes: 2_000,
  };
}

function makeAssistantMessage(content: string, inputTokens: number): Record<string, unknown> {
  return {
    timestamp: Date.now(),
    sender: { Assistant: { agent: 'tycode' } },
    content,
    reasoning: null,
    tool_calls: [],
    model_info: { model: 'MockModel' },
    token_usage: {
      input_tokens: inputTokens,
      output_tokens: 1_000,
      total_tokens: inputTokens + 1_000,
      reasoning_tokens: 0,
      cached_prompt_tokens: 0,
      cache_creation_input_tokens: 0,
    },
    context_breakdown: makeContextBreakdown(inputTokens),
    images: [],
  };
}

async function runMockResponse(conversationId: number, userMessage: string, turnCount: number): Promise<void> {
  const inputTokens = Math.min(turnCount * 50_000, 180_000);
  const response = `Mock response to: ${userMessage || '(empty message)'}`;
  const finalTypingDelayMsRaw = Number((window as any).__mockFinalTypingDelayMs);
  const finalTypingDelayMs = Number.isFinite(finalTypingDelayMsRaw)
    ? Math.max(0, Math.min(5_000, finalTypingDelayMsRaw))
    : 0;

  await emitChatEvent(conversationId, {
    kind: 'TypingStatusChanged',
    data: true,
  });

  await emitChatEvent(conversationId, {
    kind: 'StreamStart',
    data: {
      agent: 'tycode',
      model: 'mock-model',
    },
  });

  const mid = Math.max(1, Math.floor(response.length / 2));
  await emitChatEvent(conversationId, {
    kind: 'StreamDelta',
    data: { text: response.slice(0, mid) },
  });
  await sleep(25);
  await emitChatEvent(conversationId, {
    kind: 'StreamDelta',
    data: { text: response.slice(mid) },
  });
  await sleep(25);

  const message = makeAssistantMessage(response, inputTokens);
  const includeTools = (window as any).__mockIncludeToolCalls;
  const toolCallId = `mock-tool-${conversationId}-${turnCount}`;
  const failedToolId = `mock-tool-fail-${conversationId}-${turnCount}`;

  if (includeTools) {
    const toolCalls: Array<{id: string, name: string, arguments: Record<string, unknown>}> = [
      { id: toolCallId, name: 'ReadFiles', arguments: { file_paths: ['/mock/workspace/README.md'] } },
    ];
    if ((window as any).__mockToolCallFailure) {
      toolCalls.push({ id: failedToolId, name: 'ModifyFile', arguments: { file_path: '/mock/workspace/broken.ts', description: 'Fix syntax error' } });
    }
    message.tool_calls = toolCalls;
  }

  await emitChatEvent(conversationId, {
    kind: 'StreamEnd',
    data: { message },
  });

  if (includeTools) {
    await sleep(50);
    await emitChatEvent(conversationId, {
      kind: 'ToolRequest',
      data: {
        tool_call_id: toolCallId,
        tool_name: 'ReadFiles',
        tool_type: {
          kind: 'ReadFiles',
          file_paths: ['/mock/workspace/README.md'],
        },
      },
    });
    await sleep(10);
    await emitChatEvent(conversationId, {
      kind: 'ToolExecutionCompleted',
      data: {
        tool_call_id: toolCallId,
        tool_name: 'ReadFiles',
        tool_result: {
          kind: 'ReadFiles',
          files: [{ path: '/mock/workspace/README.md', bytes: 12 }],
        },
        success: true,
      },
    });

    if ((window as any).__mockToolCallFailure) {
      await emitChatEvent(conversationId, {
        kind: 'ToolRequest',
        data: {
          tool_call_id: failedToolId,
          tool_name: 'ModifyFile',
          tool_type: {
            kind: 'ModifyFile',
            file_path: '/mock/workspace/broken.ts',
            before: 'old code',
            after: 'new code',
          },
        },
      });
      await sleep(10);
      await emitChatEvent(conversationId, {
        kind: 'ToolExecutionCompleted',
        data: {
          tool_call_id: failedToolId,
          tool_name: 'ModifyFile',
          tool_result: {
            kind: 'Error',
            short_message: 'File not found: /mock/workspace/broken.ts',
            detailed_message: null,
          },
          success: false,
        },
      });
    }
  }

  if (finalTypingDelayMs > 0) {
    await sleep(finalTypingDelayMs);
  }
  await emitChatEvent(conversationId, {
    kind: 'TypingStatusChanged',
    data: false,
  });
}

export async function invoke(cmd: string, args?: any): Promise<any> {
  switch (cmd) {
    case 'create_conversation': {
      const id = nextConversationId++;
      const workspaceRoots = Array.isArray(args?.workspaceRoots)
        ? args.workspaceRoots.filter((v: unknown) => typeof v === 'string')
        : [];
      const backendKind = typeof args?.backendKind === 'string' ? args.backendKind : 'tycode';
      conversations.set(id, { workspaceRoots, turnCount: 0, backendKind });

      const sshRoot = workspaceRoots.find((r: string) => r.startsWith('ssh://'));
      if (sshRoot) {
        const host = sshRoot.slice('ssh://'.length).split('/')[0];
        const failStep = (window as any).__mockRemoteFailStep as string | undefined;
        const success = await emitRemoteProgress(host, failStep);
        if (!success) {
          throw new Error('Remote connection failed');
        }
      }

      await emitChatEvent(id, {
        kind: 'ConversationRegistered',
        data: {
          agent_id: null,
          workspace_roots: workspaceRoots,
          backend_kind: backendKind,
          name: 'Conversation',
          parent_agent_id: null,
        },
      });

      return id;
    }

    case 'send_message': {
      const conversationId = Number(args?.conversationId);
      if (!Number.isFinite(conversationId)) {
        throw new Error('Invalid conversation id');
      }
      const state = conversations.get(conversationId);
      if (!state) {
        throw new Error('No conversation found');
      }
      state.turnCount += 1;
      const text = typeof args?.message === 'string' ? args.message : '';
      await runMockResponse(conversationId, text, state.turnCount);
      return null;
    }

    case 'close_conversation': {
      const conversationId = Number(args?.conversationId);
      if (Number.isFinite(conversationId)) {
        conversations.delete(conversationId);
      }
      return null;
    }

    case 'spawn_agent': {
      const workspaceRoots = Array.isArray(args?.workspaceRoots)
        ? args.workspaceRoots.filter((v: unknown) => typeof v === 'string')
        : [];
      const prompt = typeof args?.prompt === 'string' ? args.prompt.trim() : '';
      if (workspaceRoots.length === 0) {
        throw new Error('spawn_agent requires at least one workspace root');
      }
      if (!prompt) {
        throw new Error('spawn_agent requires a non-empty prompt');
      }
      const backendKind = typeof args?.backendKind === 'string' && args.backendKind.trim()
        ? args.backendKind.trim().toLowerCase()
        : 'tycode';
      const keepAliveWithoutTab = typeof args?.keepAliveWithoutTab === 'boolean'
        ? args.keepAliveWithoutTab
        : true;
      const parentAgentId = Number.isFinite(Number(args?.parentAgentId))
        ? Number(args?.parentAgentId)
        : null;
      const completionDelayMs = Number.isFinite(Number(args?.mockCompletionDelayMs))
        ? Math.max(0, Number(args?.mockCompletionDelayMs))
        : 70;

      const conversationId = nextConversationId++;
      conversations.set(conversationId, { workspaceRoots, turnCount: 0, backendKind });

      const now = Date.now();
      const agentId = nextRuntimeAgentId++;
      const agent: MockRuntimeAgent = {
        agent_id: agentId,
        conversation_id: conversationId,
        workspace_roots: workspaceRoots,
        backend_kind: backendKind,
        parent_agent_id: parentAgentId,
        keep_alive_without_tab: keepAliveWithoutTab,
        name: typeof args?.name === 'string' && args.name.trim() ? args.name.trim() : `Agent ${agentId}`,
        status: 'queued',
        summary: 'Queued',
        created_at_ms: now,
        updated_at_ms: now,
        ended_at_ms: null,
        last_error: null,
        last_message: null,
      };
      runtimeAgents.set(agentId, agent);
      pushRuntimeEvent(agent, 'agent_spawned', 'Queued');

      void (async () => {
        await sleep(20);
        updateRuntimeAgent(agentId, 'running', 'Running...', 'stream_start');
        if (completionDelayMs === 0) return;
        await sleep(completionDelayMs);
        const finalMessage = `Mock runtime agent response: ${prompt}`;
        updateRuntimeAgent(agentId, 'completed', finalMessage, 'stream_end', { lastMessage: finalMessage });
      })();

      return { agent_id: agentId, conversation_id: conversationId };
    }

    case 'send_agent_message': {
      const agentId = Number(args?.agentId);
      const message = typeof args?.message === 'string' ? args.message.trim() : '';
      if (!Number.isFinite(agentId)) throw new Error('Invalid agent id');
      if (!message) throw new Error('send_agent_message requires a non-empty message');
      const agent = runtimeAgents.get(agentId);
      if (!agent) throw new Error(`Agent ${agentId} not found`);

      updateRuntimeAgent(agentId, 'running', 'Running...', 'typing_started');
      void (async () => {
        await sleep(60);
        const finalMessage = `Mock runtime follow-up: ${message}`;
        updateRuntimeAgent(agentId, 'completed', finalMessage, 'stream_end', { lastMessage: finalMessage });
      })();
      return null;
    }

    case 'interrupt_agent': {
      const agentId = Number(args?.agentId);
      if (!Number.isFinite(agentId)) throw new Error('Invalid agent id');
      if (!runtimeAgents.has(agentId)) throw new Error(`Agent ${agentId} not found`);
      updateRuntimeAgent(agentId, 'cancelled', 'Operation cancelled', 'operation_cancelled');
      return null;
    }

    case 'terminate_agent': {
      const agentId = Number(args?.agentId);
      if (!Number.isFinite(agentId)) throw new Error('Invalid agent id');
      if (!runtimeAgents.has(agentId)) throw new Error(`Agent ${agentId} not found`);
      updateRuntimeAgent(agentId, 'cancelled', 'Terminated', 'agent_closed');
      return null;
    }

    case 'get_agent': {
      const agentId = Number(args?.agentId);
      if (!Number.isFinite(agentId)) return null;
      const agent = runtimeAgents.get(agentId);
      if (!agent) return null;
      return { ...agent, workspace_roots: [...agent.workspace_roots] };
    }

    case 'list_agents':
      return sortedRuntimeAgents();

    case 'wait_for_agent': {
      const agentId = Number(args?.agentId);
      const untilRaw = typeof args?.until === 'string' ? args.until.trim().toLowerCase() : '';
      const timeoutMs = Number.isFinite(Number(args?.timeoutMs))
        ? Math.max(1, Math.min(30 * 60 * 1000, Number(args?.timeoutMs)))
        : 60_000;
      if (!Number.isFinite(agentId)) throw new Error('Invalid agent id');

      type WaitUntil = 'idle' | 'terminal' | 'completed' | 'failed' | 'needs_input';
      const waitUntil: WaitUntil = (() => {
        if (untilRaw === '' || untilRaw === 'idle') return 'idle';
        if (untilRaw === 'terminal' || untilRaw === 'done') return 'terminal';
        if (untilRaw === 'completed' || untilRaw === 'complete') return 'completed';
        if (untilRaw === 'failed' || untilRaw === 'error') return 'failed';
        if (
          untilRaw === 'needs_input'
          || untilRaw === 'needs-input'
          || untilRaw === 'waiting_input'
          || untilRaw === 'waiting-input'
          || untilRaw === 'input'
        ) {
          return 'needs_input';
        }
        throw new Error(
          `Unsupported wait condition '${untilRaw}'. Supported values: idle, terminal, completed, failed, needs_input`,
        );
      })();

      const met = (status: MockRuntimeAgentStatus): boolean => {
        if (waitUntil === 'idle') return status !== 'queued' && status !== 'running';
        if (waitUntil === 'completed') return status === 'completed';
        if (waitUntil === 'failed') return status === 'failed';
        if (waitUntil === 'needs_input') return status === 'waiting_input';
        return isTerminalRuntimeStatus(status); // terminal
      };

      const deadline = Date.now() + timeoutMs;
      while (Date.now() <= deadline) {
        const agent = runtimeAgents.get(agentId);
        if (!agent) throw new Error(`Agent ${agentId} not found`);
        if (met(agent.status)) {
          return { ...agent, workspace_roots: [...agent.workspace_roots] };
        }
        await sleep(20);
      }
      throw new Error(`Timed out waiting for agent ${agentId}`);
    }

    case 'agent_events_since': {
      const sinceSeq = Number.isFinite(Number(args?.sinceSeq)) ? Number(args?.sinceSeq) : 0;
      const limit = Number.isFinite(Number(args?.limit))
        ? Math.max(1, Math.min(1000, Number(args?.limit)))
        : 200;
      const events = runtimeEvents
        .filter((event) => event.seq > sinceSeq)
        .slice(0, limit)
        .map((event) => ({ ...event }));
      const latestSeq = runtimeEvents.length > 0
        ? runtimeEvents[runtimeEvents.length - 1].seq
        : 0;
      return {
        events,
        latest_seq: latestSeq,
      };
    }

    case 'collect_agent_result': {
      const agentId = Number(args?.agentId);
      if (!Number.isFinite(agentId)) throw new Error('Invalid agent id');
      const agent = runtimeAgents.get(agentId);
      if (!agent) throw new Error(`Agent ${agentId} not found`);
      return {
        agent: { ...agent, workspace_roots: [...agent.workspace_roots] },
        final_message: agent.last_message ?? null,
        changed_files: [],
        tool_results: [],
      };
    }

    case 'get_mcp_http_server_settings':
      syncMockMcpSettingsFromStorage();
      return {
        enabled: mockMcpHttpServerEnabled,
        running: mockMcpHttpServerEnabled,
        url: mockMcpHttpServerEnabled ? mockMcpHttpServerUrl : null,
      };

    case 'set_mcp_http_server_enabled': {
      if (typeof args?.enabled !== 'boolean') {
        throw new Error('set_mcp_http_server_enabled requires boolean enabled');
      }
      mockMcpHttpServerEnabled = args.enabled;
      try {
        window.localStorage.setItem('mock-mcp-http-enabled', String(mockMcpHttpServerEnabled));
      } catch {
        // Ignore storage access failures in tests.
      }
      return {
        enabled: mockMcpHttpServerEnabled,
        running: mockMcpHttpServerEnabled,
        url: mockMcpHttpServerEnabled ? mockMcpHttpServerUrl : null,
      };
    }

    case 'get_driver_mcp_http_server_settings':
      return {
        enabled: mockDriverMcpHttpServerEnabled,
        autoload: mockDriverMcpHttpServerAutoload,
        running: mockDriverMcpHttpServerEnabled,
        url: mockDriverMcpHttpServerEnabled ? mockDriverMcpHttpServerUrl : null,
      };

    case 'set_driver_mcp_http_server_enabled': {
      if (typeof args?.enabled !== 'boolean') {
        throw new Error('set_driver_mcp_http_server_enabled requires boolean enabled');
      }
      mockDriverMcpHttpServerEnabled = args.enabled;
      if (!mockDriverMcpHttpServerEnabled) {
        mockDriverMcpHttpServerAutoload = false;
      }
      return {
        enabled: mockDriverMcpHttpServerEnabled,
        autoload: mockDriverMcpHttpServerAutoload,
        running: mockDriverMcpHttpServerEnabled,
        url: mockDriverMcpHttpServerEnabled ? mockDriverMcpHttpServerUrl : null,
      };
    }

    case 'set_driver_mcp_http_server_autoload_enabled': {
      if (typeof args?.enabled !== 'boolean') {
        throw new Error('set_driver_mcp_http_server_autoload_enabled requires boolean enabled');
      }
      if (args.enabled && !mockDriverMcpHttpServerEnabled) {
        throw new Error('Enable driver MCP server before enabling auto-load');
      }
      mockDriverMcpHttpServerAutoload = args.enabled && mockDriverMcpHttpServerEnabled;
      return {
        enabled: mockDriverMcpHttpServerEnabled,
        autoload: mockDriverMcpHttpServerAutoload,
        running: mockDriverMcpHttpServerEnabled,
        url: mockDriverMcpHttpServerEnabled ? mockDriverMcpHttpServerUrl : null,
      };
    }

    case 'submit_debug_ui_response':
      return null;

    case 'create_terminal': {
      const workspacePath = typeof args?.workspacePath === 'string' ? args.workspacePath : '';
      const id = nextTerminalId++;
      terminalWorkspaceById.set(id, workspacePath);
      return id;
    }

    case 'write_terminal':
    case 'resize_terminal':
      return null;

    case 'close_terminal': {
      const terminalId = Number(args?.terminalId);
      if (Number.isFinite(terminalId)) {
        terminalWorkspaceById.delete(terminalId);
      }
      return null;
    }

    case 'list_active_conversations':
      return Array.from(conversations.keys());

    case 'get_settings': {
      const cid = Number(args?.conversationId);
      if (Number.isFinite(cid)) {
        await emitChatEvent(cid, {
          kind: 'Settings',
          data: {
            model_quality: 'unlimited',
            review_level: 'Task',
            default_agent: 'one_shot',
            communication_tone: 'concise_and_logical',
            reasoning_effort: 'Medium',
            autonomy_level: 'plan_approval_required',
            enable_type_analyzer: true,
            spawn_context_mode: 'Fork',
            run_build_test_output_mode: 'ToolResponse',
            disable_custom_steering: false,
            disable_streaming: false,
            active_provider: 'MockProvider',
            providers: {
              MockProvider: {
                type: 'openrouter',
                api_key: 'sk-mock',
              },
            },
            mcp_servers: {},
            agent_models: {},
            modules: {},
          },
        });
      }
      return {};
    }

    case 'list_profiles': {
      const cid = Number(args?.conversationId);
      if (Number.isFinite(cid)) {
        await emitChatEvent(cid, {
          kind: 'ProfilesList',
          data: {
            profiles: ['default', 'work'],
            active_profile: 'default',
          },
        });
      }
      return [];
    }

    case 'list_sessions':
      return [];

    case 'list_directory': {
      const path = typeof args?.path === 'string' ? args.path : '';
      if (!path) return [];
      return [
        {
          name: 'README.md',
          path: `${path.replace(/\/$/, '')}/README.md`,
          is_directory: false,
          size: 512,
        },
      ];
    }

    case 'read_file_content': {
      const path = typeof args?.path === 'string' ? args.path : '/mock/workspace/README.md';
      const overrides = (window as any).__mockReadFileContentByPath as Record<string, string> | undefined;
      const overriddenContent = overrides?.[path];
      const content = typeof overriddenContent === 'string' ? overriddenContent : '# Mock file\n';
      return {
        path,
        content,
        size: content.length,
        truncated: false,
      };
    }

    case 'git_current_branch':
      if ((window as any).__mockGitNotRepo) {
        throw new Error('git rev-parse --abbrev-ref HEAD: fatal: not a git repository (or any of the parent directories): .git');
      }
      return 'main';

    case 'git_status':
      if ((window as any).__mockGitNotRepo) {
        throw new Error('git status: fatal: not a git repository (or any of the parent directories): .git');
      }
      return [];

    case 'git_worktree_add':
    case 'git_worktree_remove':
      return null;

    case 'git_stage':
    case 'git_unstage':
    case 'git_commit':
    case 'git_diff':
    case 'git_discard':
    case 'get_session_id': {
      const cid = Number(args?.conversationId);
      const conv = conversations.get(cid);
      if (!conv) return null;
      // Return a deterministic session_id based on backend kind + conversation id
      return `${conv.backendKind}-session-${cid}`;
    }

    case 'cancel_conversation':
    case 'resume_session':
    case 'delete_session':
    case 'export_session_json':
    case 'get_module_schemas': {
      const cid = Number(args?.conversationId);
      if (Number.isFinite(cid)) {
        await emitChatEvent(cid, {
          kind: 'ModuleSchemas',
          data: {
            schemas: [
              {
                namespace: 'execution',
                schema: {
                  title: 'Execution',
                  description: 'Execution module settings',
                  type: 'object',
                  properties: {
                    timeout_seconds: {
                      type: 'integer',
                      description: 'Command timeout in seconds',
                      default: 300,
                    },
                    sandbox_enabled: {
                      type: 'boolean',
                      description: 'Enable sandboxed execution',
                      default: false,
                    },
                  },
                },
              },
            ],
          },
        });
      }
      return null;
    }
    case 'switch_profile':
    case 'update_settings':
    case 'restart_subprocess':
    case 'plugin:opener|reveal_item_in_dir':
    case 'plugin:shell|open':
      return null;

    case 'create_admin_subprocess': {
      const id = nextAdminId++;
      const workspaceRoots = Array.isArray(args?.workspaceRoots)
        ? args.workspaceRoots.filter((v: unknown) => typeof v === 'string')
        : [];
      const backendKind = typeof args?.backendKind === 'string' && args.backendKind.trim()
        ? args.backendKind.trim().toLowerCase()
        : 'tycode';
      adminSubprocesses.set(id, { workspaceRoots, backendKind });
      return id;
    }

    case 'close_admin_subprocess': {
      const adminId = Number(args?.adminId);
      if (Number.isFinite(adminId)) adminSubprocesses.delete(adminId);
      return null;
    }

    case 'admin_list_sessions': {
      const adminId = Number(args?.adminId);
      if (Number.isFinite(adminId)) {
        const adminState = adminSubprocesses.get(adminId);
        const backendKind = adminState?.backendKind ?? 'tycode';
        const sessions = (mockSessionsByBackend[backendKind] ?? []).map((session) => ({ ...session }));
        await emitAdminEvent(adminId, {
          kind: 'SessionsList',
          data: {
            sessions,
          },
        });
      }
      return null;
    }

    case 'admin_get_settings': {
      const adminId = Number(args?.adminId);
      if (Number.isFinite(adminId)) {
        await emitAdminEvent(adminId, {
          kind: 'Settings',
          data: {
            model_quality: 'unlimited',
            review_level: 'Task',
            default_agent: 'one_shot',
            communication_tone: 'concise_and_logical',
            reasoning_effort: 'Medium',
            autonomy_level: 'plan_approval_required',
            enable_type_analyzer: true,
            spawn_context_mode: 'Fork',
            run_build_test_output_mode: 'ToolResponse',
            disable_custom_steering: false,
            disable_streaming: false,
            active_provider: 'MockProvider',
            providers: {
              MockProvider: {
                type: 'openrouter',
                api_key: 'sk-mock',
              },
            },
            mcp_servers: {},
            agent_models: {},
            modules: {},
          },
        });
      }
      return null;
    }

    case 'admin_list_profiles': {
      const adminId = Number(args?.adminId);
      if (Number.isFinite(adminId)) {
        await emitAdminEvent(adminId, {
          kind: 'ProfilesList',
          data: { profiles: ['default', 'work'], active_profile: 'default' },
        });
      }
      return null;
    }

    case 'admin_get_module_schemas': {
      const adminId = Number(args?.adminId);
      if (Number.isFinite(adminId)) {
        await emitAdminEvent(adminId, {
          kind: 'ModuleSchemas',
          data: {
            schemas: [{
              namespace: 'execution',
              schema: {
                title: 'Execution',
                description: 'Execution module settings',
                type: 'object',
                properties: {
                  timeout_seconds: { type: 'integer', description: 'Command timeout in seconds', default: 300 },
                  sandbox_enabled: { type: 'boolean', description: 'Enable sandboxed execution', default: false },
                },
              },
            }],
          },
        });
      }
      return null;
    }

    case 'admin_update_settings':
    case 'admin_switch_profile':
      return null;

    case 'admin_delete_session': {
      const adminId = Number(args?.adminId);
      const sessionId = typeof args?.sessionId === 'string' ? args.sessionId : '';
      if (Number.isFinite(adminId) && sessionId) {
        const adminState = adminSubprocesses.get(adminId);
        const backendKind = adminState?.backendKind ?? 'tycode';
        const current = mockSessionsByBackend[backendKind] ?? [];
        mockSessionsByBackend[backendKind] = current.filter((session) => session.id !== sessionId);
      }
      return null;
    }

    default:
      return null;
  }
}

export function transformCallback(_callback: Function, _once?: boolean): number {
  return 0;
}

// Expose invoke on window so E2E tests can call it via browser.execute()
(window as any).__TAURI_INTERNALS__ = { invoke };
