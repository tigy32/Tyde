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

interface MockRuntimeAgent {
  agent_id: number;
  conversation_id: number;
  workspace_roots: string[];
  backend_kind: string;
  parent_agent_id: number | null;
  keep_alive_without_tab: boolean;
  name: string;
  is_running: boolean;
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
  is_running: boolean;
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

interface MockSessionRecord {
  id: string;
  backend_session_id: string | null;
  backend_kind: string;
  alias: string | null;
  user_alias: string | null;
  parent_id: string | null;
  workspace_root: string | null;
  created_at_ms: number;
  updated_at_ms: number;
  message_count: number;
}

let nextMockRecordId = 1;
const mockSessionRecords = new Map<string, MockSessionRecord>();
const conversationToRecordId = new Map<number, string>();

let nextTerminalId = 70_000;
const terminalWorkspaceById = new Map<number, string>();
let mockMcpHttpServerEnabled = true;
const mockMcpHttpServerUrl = 'http://127.0.0.1:47771/mcp';
let mockDriverMcpHttpServerEnabled = false;
let mockDriverMcpHttpServerAutoload = false;
const mockDriverMcpHttpServerUrl = 'http://127.0.0.1:47772/mcp';

interface MockHost {
  id: string;
  label: string;
  hostname: string;
  is_local: boolean;
  enabled_backends: string[];
  default_backend: string;
}

const mockHosts: MockHost[] = [
  {
    id: 'local',
    label: 'Local',
    hostname: '',
    is_local: true,
    enabled_backends: ['tycode', 'codex', 'claude', 'kiro'],
    default_backend: 'tycode',
  },
];

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

function pushRuntimeEvent(agent: MockRuntimeAgent, kind: string, message: string | null): void {
  runtimeEvents.push({
    seq: nextRuntimeEventSeq++,
    agent_id: agent.agent_id,
    conversation_id: agent.conversation_id,
    kind,
    is_running: agent.is_running,
    timestamp_ms: Date.now(),
    message,
  });
  if (runtimeEvents.length > 5000) {
    runtimeEvents.splice(0, runtimeEvents.length - 5000);
  }
}

function updateRuntimeAgent(
  agentId: number,
  isRunning: boolean,
  summary: string,
  kind: string,
  options?: { lastMessage?: string | null; lastError?: string | null },
): void {
  const agent = runtimeAgents.get(agentId);
  if (!agent) return;
  agent.is_running = isRunning;
  agent.summary = summary;
  agent.updated_at_ms = Date.now();
  agent.last_message = options?.lastMessage ?? agent.last_message;
  agent.last_error = options?.lastError ?? agent.last_error;
  agent.ended_at_ms = isRunning ? null : Date.now();
  pushRuntimeEvent(agent, kind, summary);
  emit('agent-changed', { ...agent, workspace_roots: [...agent.workspace_roots] });
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

      const now = Date.now();
      const recordId = `mock-record-${nextMockRecordId++}`;
      const record: MockSessionRecord = {
        id: recordId,
        backend_session_id: null,
        backend_kind: backendKind,
        alias: null,
        user_alias: null,
        parent_id: null,
        workspace_root: workspaceRoots[0] ?? null,
        created_at_ms: now,
        updated_at_ms: now,
        message_count: 0,
      };
      mockSessionRecords.set(recordId, record);
      conversationToRecordId.set(id, recordId);

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

      return { conversation_id: id, session_id: recordId };
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
        is_running: false,
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
        updateRuntimeAgent(agentId, true, 'Running...', 'stream_start');
        if (completionDelayMs === 0) return;
        await sleep(completionDelayMs);
        const finalMessage = `Mock runtime agent response: ${prompt}`;
        updateRuntimeAgent(agentId, false, finalMessage, 'typing_stopped', { lastMessage: finalMessage });
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

      updateRuntimeAgent(agentId, true, 'Running...', 'typing_started');
      void (async () => {
        await sleep(60);
        const finalMessage = `Mock runtime follow-up: ${message}`;
        updateRuntimeAgent(agentId, false, finalMessage, 'typing_stopped', { lastMessage: finalMessage });
      })();
      return null;
    }

    case 'interrupt_agent': {
      const agentId = Number(args?.agentId);
      if (!Number.isFinite(agentId)) throw new Error('Invalid agent id');
      if (!runtimeAgents.has(agentId)) throw new Error(`Agent ${agentId} not found`);
      updateRuntimeAgent(agentId, false, 'Operation cancelled', 'operation_cancelled');
      return null;
    }

    case 'terminate_agent': {
      const agentId = Number(args?.agentId);
      if (!Number.isFinite(agentId)) throw new Error('Invalid agent id');
      if (!runtimeAgents.has(agentId)) throw new Error(`Agent ${agentId} not found`);
      updateRuntimeAgent(agentId, false, 'Terminated', 'agent_closed');
      return null;
    }

    case 'rename_agent': {
      const agentId = Number(args?.agentId);
      const name = typeof args?.name === 'string' ? args.name.trim() : '';
      if (!Number.isFinite(agentId)) throw new Error('Invalid agent id');
      const agent = runtimeAgents.get(agentId);
      if (!agent) throw new Error(`Agent ${agentId} not found`);
      if (name) agent.name = name;
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
      const timeoutMs = Number.isFinite(Number(args?.timeoutMs))
        ? Math.max(1, Math.min(30 * 60 * 1000, Number(args?.timeoutMs)))
        : 60_000;
      if (!Number.isFinite(agentId)) throw new Error('Invalid agent id');

      const deadline = Date.now() + timeoutMs;
      while (Date.now() <= deadline) {
        const agent = runtimeAgents.get(agentId);
        if (!agent) throw new Error(`Agent ${agentId} not found`);
        if (!agent.is_running) {
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

    case 'set_default_backend':
      return null;

    case 'query_backend_usage': {
      const backendKind = typeof args?.backendKind === 'string' ? args.backendKind : 'codex';
      const hostId = typeof args?.hostId === 'string' ? args.hostId : 'local';
      return {
        backend_kind: backendKind,
        source: hostId === 'local' ? 'local' : `remote:${hostId}`,
        captured_at_ms: Date.now(),
        plan: hostId === 'local' ? 'Personal' : 'Work',
        status: null,
        windows: [
          {
            id: 'primary',
            label: '5-hour',
            used_percent: hostId === 'local' ? 35 : 62,
            reset_at_text: null,
            reset_at_unix: null,
            window_minutes: 300,
          },
        ],
        details: [],
      };
    }

    case 'list_hosts':
      return mockHosts.map((h) => ({ ...h }));

    case 'add_host': {
      const label = typeof args?.label === 'string' ? args.label : '';
      const hostname = typeof args?.hostname === 'string' ? args.hostname : '';
      const newHost: MockHost = {
        id: `mock-host-${Date.now()}`,
        label,
        hostname,
        is_local: false,
        enabled_backends: ['tycode', 'codex', 'claude', 'kiro'],
        default_backend: 'tycode',
      };
      mockHosts.push(newHost);
      return { ...newHost };
    }

    case 'remove_host': {
      const id = typeof args?.id === 'string' ? args.id : '';
      const idx = mockHosts.findIndex((h) => h.id === id);
      if (idx >= 0 && !mockHosts[idx].is_local) {
        mockHosts.splice(idx, 1);
      }
      return null;
    }

    case 'update_host_label': {
      const id = typeof args?.id === 'string' ? args.id : '';
      const label = typeof args?.label === 'string' ? args.label : '';
      const host = mockHosts.find((h) => h.id === id);
      if (host) host.label = label;
      return null;
    }

    case 'update_host_enabled_backends': {
      const id = typeof args?.id === 'string' ? args.id : '';
      const backends = Array.isArray(args?.backends) ? args.backends : [];
      const host = mockHosts.find((h) => h.id === id);
      if (host) host.enabled_backends = backends;
      return null;
    }

    case 'update_host_default_backend': {
      const id = typeof args?.id === 'string' ? args.id : '';
      const backend = typeof args?.backend === 'string' ? args.backend : '';
      const host = mockHosts.find((h) => h.id === id);
      if (host) host.default_backend = backend;
      return null;
    }

    case 'set_mcp_control_enabled':
      return null;

    case 'get_host_for_workspace': {
      const workspacePath = typeof args?.workspacePath === 'string' ? args.workspacePath : '';
      if (workspacePath.startsWith('ssh://')) {
        const hostname = workspacePath.slice('ssh://'.length).split('/')[0];
        const found = mockHosts.find((h) => h.hostname === hostname);
        if (found) return { ...found };
      }
      return { ...mockHosts[0] };
    }

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

    case 'list_session_records':
      return Array.from(mockSessionRecords.values()).map((r) => ({ ...r }));

    case 'rename_session': {
      const recordId = typeof args?.id === 'string' ? args.id : '';
      const newName = typeof args?.name === 'string' ? args.name : '';
      const rec = mockSessionRecords.get(recordId);
      if (rec) {
        rec.user_alias = newName || null;
        rec.updated_at_ms = Date.now();
      }
      return null;
    }

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

    case 'discover_git_repos': {
      const workspaceDir = typeof args?.workspaceDir === 'string' ? args.workspaceDir : '';
      if ((window as any).__mockGitNotRepo) {
        return [];
      }
      return workspaceDir ? [workspaceDir] : [];
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
      const backendSessionId = `${conv.backendKind}-session-${cid}`;
      // Mirror real backend: set backend_session_id on the corresponding record
      const linkedRecordId = conversationToRecordId.get(cid);
      if (linkedRecordId) {
        const rec = mockSessionRecords.get(linkedRecordId);
        if (rec && !rec.backend_session_id) {
          rec.backend_session_id = backendSessionId;
          rec.updated_at_ms = Date.now();
        }
      }
      return backendSessionId;
    }

    case 'resume_session': {
      const resumeCid = Number(args?.conversationId);
      const resumeSessionId = typeof args?.sessionId === 'string' ? args.sessionId : '';
      // Mirror real backend: set backend_session_id on the record created by create_conversation
      const resumeRecordId = conversationToRecordId.get(resumeCid);
      if (resumeRecordId && resumeSessionId) {
        const rec = mockSessionRecords.get(resumeRecordId);
        if (rec && !rec.backend_session_id) {
          rec.backend_session_id = resumeSessionId;
          rec.updated_at_ms = Date.now();
        }
      }
      return null;
    }

    case 'cancel_conversation':
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

    case 'list_workflows': {
      const workflows = (window as any).__mockWorkflows as Array<{
        id: string;
        name: string;
        description: string;
        trigger: string;
        steps: Array<{ name: string; actions: Array<Record<string, unknown>> }>;
        scope: string;
      }> | undefined;
      return workflows ?? [];
    }

    case 'save_workflow': {
      const json = typeof args?.workflowJson === 'string' ? args.workflowJson : '{}';
      const parsed = JSON.parse(json);
      const scope = typeof args?.scope === 'string' ? args.scope : 'global';
      const workflows = ((window as any).__mockWorkflows ?? []) as Array<Record<string, unknown>>;
      const existing = workflows.findIndex((w) => w.id === parsed.id);
      if (existing >= 0) {
        workflows[existing] = { ...parsed, scope };
      } else {
        workflows.push({ ...parsed, scope });
      }
      (window as any).__mockWorkflows = workflows;
      return null;
    }

    case 'delete_workflow': {
      const id = typeof args?.id === 'string' ? args.id : '';
      const workflows = ((window as any).__mockWorkflows ?? []) as Array<Record<string, unknown>>;
      (window as any).__mockWorkflows = workflows.filter((w) => w.id !== id);
      return null;
    }

    case 'run_shell_command': {
      const command = typeof args?.command === 'string' ? args.command : '';
      const mockShellHandler = (window as any).__mockShellCommandHandler as
        | ((cmd: string) => { stdout: string; stderr: string; exit_code: number | null; success: boolean })
        | undefined;
      if (mockShellHandler) {
        return mockShellHandler(command);
      }
      return {
        stdout: `mock output for: ${command}\n`,
        stderr: '',
        exit_code: 0,
        success: true,
      };
    }

    default:
      return null;
  }
}

export function transformCallback(_callback: Function, _once?: boolean): number {
  return 0;
}

// Expose a helper so E2E tests can inject/update mock runtime agents directly.
(window as any).__mockSetRuntimeAgent = (agent: MockRuntimeAgent) => {
  runtimeAgents.set(agent.agent_id, agent);
  emit('agent-changed', { ...agent, workspace_roots: [...agent.workspace_roots] });
};

// Expose invoke on window so E2E tests can call it via browser.execute()
(window as any).__TAURI_INTERNALS__ = { invoke };
