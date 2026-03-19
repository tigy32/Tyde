import type { ChatEvent, ChatMessage } from "@tyde/protocol";
import type { AgentsPanel } from "./agents";
import type { ChatPanel } from "./chat";
import type { DiffPanel } from "./diff_panel";
import type { GitPanel } from "./git";
import type { NotificationManager } from "./notifications";
import type { SessionsPanel } from "./sessions";
import type { TabManager } from "./tabs";

interface EventRouterDeps {
  chatPanel: ChatPanel;
  tabManager: TabManager;
  gitPanel: GitPanel;
  sessionsPanel: SessionsPanel;
  notifications: NotificationManager;
  agentsPanel: AgentsPanel;
  diffPanel: DiffPanel;
}

function assertUnreachable(x: never): never {
  throw new Error(`Unhandled event kind: ${JSON.stringify(x)}`);
}

function summarizeText(content: string, fallback: string): string {
  const compact = content.replace(/\s+/g, " ").trim();
  if (!compact) return fallback;
  return compact.length > 120 ? `${compact.slice(0, 117)}...` : compact;
}

function summarizeTaskTitle(title: string): string {
  const compact = title.replace(/\s+/g, " ").trim();
  if (!compact) return "";

  const withoutPrefix = compact.replace(
    /^(?:task(?:\s+list)?|tasks)\s*[:-]\s*/i,
    "",
  );
  const words =
    (withoutPrefix || compact).match(/[A-Za-z0-9]+(?:['-][A-Za-z0-9]+)*/g) ??
    [];
  if (words.length === 0) return "";
  return words.slice(0, 3).join(" ");
}

// Routes incoming chat events from the Tauri backend to the appropriate UI components.
export class EventRouter {
  private deps: EventRouterDeps;
  private sessionsLoadTimeout: number | null = null;
  private feedbackAgentFiles: Map<number, string> = new Map();
  private waitingForUserInput: Set<number> = new Set();
  private typingByConversation: Map<number, boolean> = new Map();
  onRefreshFile: ((filePath: string) => void) | null = null;

  constructor(deps: EventRouterDeps) {
    this.deps = deps;
  }

  beginSessionsLoading(): void {
    this.deps.sessionsPanel.showLoading();
    if (this.sessionsLoadTimeout !== null) {
      clearTimeout(this.sessionsLoadTimeout);
    }
    this.sessionsLoadTimeout = window.setTimeout(() => {
      this.sessionsLoadTimeout = null;
      this.deps.sessionsPanel.showError(
        "Session list request timed out. Try refreshing.",
      );
    }, 10_000);
  }

  clearSessionsLoadingTimeout(): void {
    if (this.sessionsLoadTimeout !== null) {
      clearTimeout(this.sessionsLoadTimeout);
      this.sessionsLoadTimeout = null;
    }
  }

  registerFeedbackAgent(conversationId: number, filePath: string): void {
    this.feedbackAgentFiles.set(conversationId, filePath);
  }

  unregisterFeedbackAgent(conversationId: number): void {
    this.feedbackAgentFiles.delete(conversationId);
  }

  handleChatEvent(payload: {
    conversation_id: number;
    event: ChatEvent;
  }): void {
    const {
      chatPanel,
      tabManager,
      gitPanel,
      sessionsPanel,
      notifications,
      agentsPanel,
    } = this.deps;
    const conversationId = payload.conversation_id;
    const tab = tabManager.getTabByConversationId(conversationId);
    const agent = agentsPanel.getAgentByConversationId(conversationId);

    const event = payload.event;
    const isActiveTab =
      tab !== null && tab.id === tabManager.getActiveTab()?.id;

    chatPanel.handleConversationEvent(conversationId, event);
    this.handleFeedbackAgentEvent(payload);

    switch (event.kind) {
      case "StreamStart":
        this.typingByConversation.set(conversationId, true);
        this.waitingForUserInput.delete(conversationId);
        if (tab) tabManager.setStreaming(tab.id, true);
        if (agent) {
          agentsPanel.updateAgent(conversationId, {
            isTyping: true,
            summary: "Running...",
          });
        }
        break;
      case "StreamDelta":
        break;
      case "StreamReasoningDelta":
        break;
      case "StreamEnd": {
        if (tab) tabManager.setStreaming(tab.id, false);
        if (tab && !isActiveTab) {
          tabManager.markUnread(tab.id);
        }
        this.syncContextUsage(conversationId, event.data.message);
        if (agent) {
          agentsPanel.updateAgent(conversationId, {
            summary: summarizeText(
              event.data.message.content,
              "Response complete",
            ),
          });
        }
        break;
      }
      case "TimingUpdate":
        break;
      case "TypingStatusChanged":
        // TypingStatusChanged=false is authoritative for "agent is no longer running".
        // Treat any non-true payload as false to avoid truthy/non-boolean glitches.
        this.typingByConversation.set(conversationId, event.data === true);
        if (agent) {
          if (event.data === true) {
            agentsPanel.updateAgent(conversationId, {
              isTyping: true,
              summary: "Running...",
            });
          } else {
            const waiting = this.waitingForUserInput.has(conversationId);
            const normalizedSummary = agent.summary.trim().toLowerCase();
            const staleRunningSummary =
              normalizedSummary === "" ||
              normalizedSummary === "running..." ||
              normalizedSummary.startsWith("using ");
            agentsPanel.updateAgent(conversationId, {
              isTyping: false,
              summary: waiting
                ? "Waiting for your input"
                : staleRunningSummary
                  ? "Completed"
                  : agent.summary,
            });
          }
        }
        break;
      case "ToolExecutionCompleted":
        if (isActiveTab) gitPanel.requestRefresh();
        if (agent) {
          const failureSummary = summarizeText(
            event.data.error ??
              (event.data.tool_result.kind === "Error"
                ? event.data.tool_result.short_message
                : `Tool "${event.data.tool_name}" failed`),
            "Tool execution failed",
          );
          agentsPanel.updateAgent(conversationId, {
            ...(event.data.success ? {} : { hasError: true }),
            summary: event.data.success
              ? `Completed ${event.data.tool_name}`
              : failureSummary,
          });
        }
        break;
      case "SessionsList":
        this.clearSessionsLoadingTimeout();
        try {
          sessionsPanel.update(event.data.sessions);
        } catch (err) {
          sessionsPanel.showError("Failed to render sessions list.");
          notifications.error(`Sessions rendering failed: ${String(err)}`);
        }
        break;
      case "SubprocessExit":
        if (tab) tabManager.setStreaming(tab.id, false);
        this.waitingForUserInput.delete(conversationId);
        this.typingByConversation.set(conversationId, false);
        if (event.data.exit_code !== 0) {
          notifications.error("AI backend disconnected");
        }
        if (agent) {
          if (event.data.exit_code !== 0) {
            agentsPanel.updateAgent(conversationId, {
              isTyping: false,
              hasError: true,
              summary:
                event.data.exit_code === null
                  ? "Backend exited unexpectedly"
                  : `Backend exited (${event.data.exit_code})`,
            });
          } else {
            agentsPanel.updateAgent(conversationId, {
              isTyping: false,
              summary: summarizeText(agent.summary, "Completed"),
            });
          }
        }
        this.unregisterFeedbackAgent(conversationId);
        break;
      case "Error":
        if (tab) tabManager.setStreaming(tab.id, false);

        notifications.error(event.data);
        this.waitingForUserInput.delete(conversationId);
        this.typingByConversation.set(conversationId, false);
        if (agent) {
          agentsPanel.updateAgent(conversationId, {
            isTyping: false,
            hasError: true,
            summary: event.data,
          });
        }
        this.unregisterFeedbackAgent(conversationId);
        break;

      case "TaskUpdate":
        chatPanel.updateTaskList(conversationId, event.data);
        tabManager.autoRenameChatTab(
          conversationId,
          summarizeTaskTitle(event.data.title),
        );
        break;
      case "Settings":
        chatPanel.handleSettingsUpdate(conversationId, event.data);
        break;
      case "ProfilesList":
        chatPanel.handleProfilesList(conversationId, event.data);
        break;
      case "ModelsList":
        chatPanel.handleModelsList(conversationId, event.data);
        break;
      case "ModuleSchemas":
        break;
      case "MessageAdded":
        this.syncContextUsage(conversationId, event.data);
        if (agent) {
          agentsPanel.updateAgent(conversationId, {
            summary: summarizeText(event.data.content, agent.summary),
          });
        }
        break;
      case "ConversationCleared":
        this.waitingForUserInput.delete(conversationId);
        this.typingByConversation.set(conversationId, false);
        if (agent) {
          agentsPanel.updateAgent(conversationId, {
            isTyping: false,
          });
        }
        break;
      case "ToolRequest": {
        const isQuestion =
          event.data.tool_name === "ask_user_question" ||
          event.data.tool_name === "AskUserQuestion";
        const isPlanDone = event.data.tool_name === "ExitPlanMode";
        if (isQuestion) {
          this.waitingForUserInput.add(conversationId);
        }
        if (agent) {
          agentsPanel.updateAgent(conversationId, {
            summary: isQuestion
              ? "Waiting for your input"
              : isPlanDone
                ? "Plan ready"
                : `Using ${event.data.tool_name}...`,
          });
        }
        break;
      }
      case "OperationCancelled":
      case "RetryAttempt":
      case "SubprocessStderr":
        break;
      default:
        assertUnreachable(event);
    }
  }

  private handleFeedbackAgentEvent(payload: {
    conversation_id: number;
    event: ChatEvent;
  }): void {
    if (!this.feedbackAgentFiles.has(payload.conversation_id)) return;

    const diffPanel = this.deps.diffPanel;
    const event = payload.event;
    const convId = payload.conversation_id;

    switch (event.kind) {
      case "StreamStart":
        diffPanel.updateFeedbackProgress(convId, "Processing...", "progress");
        break;
      case "StreamDelta":
        break;
      case "ToolRequest":
        diffPanel.updateFeedbackProgress(
          convId,
          `Using tool: ${event.data.tool_name}...`,
          "progress",
        );
        break;
      case "ToolExecutionCompleted": {
        diffPanel.updateFeedbackProgress(
          convId,
          "Applying changes...",
          "progress",
        );
        const filePath = this.feedbackAgentFiles.get(convId);
        if (filePath && this.onRefreshFile) this.onRefreshFile(filePath);
        break;
      }
      case "StreamEnd":
        diffPanel.updateFeedbackProgress(
          convId,
          "Response received",
          "progress",
        );
        break;
      case "TypingStatusChanged":
        // Do NOT unregister here — the backend can emit TypingStatusChanged(false)
        // before ToolRequest(ask_user_question) in approval flows (see codex.rs).
        // Unregistering prematurely would drop the ToolRequest and any later events.
        // Only SubprocessExit and Error are truly terminal.
        if (event.data !== true) {
          diffPanel.updateFeedbackProgress(
            convId,
            "Agent finished",
            "complete",
          );
          const filePath = this.feedbackAgentFiles.get(convId);
          if (filePath && this.onRefreshFile) this.onRefreshFile(filePath);
        }
        break;
      case "SubprocessExit":
        diffPanel.updateFeedbackProgress(convId, "Agent finished", "complete");
        this.unregisterFeedbackAgent(convId);
        break;
      case "Error":
        diffPanel.updateFeedbackProgress(
          convId,
          `Error: ${event.data}`,
          "error",
        );
        this.unregisterFeedbackAgent(convId);
        break;
      default:
        break;
    }
  }

  private syncContextUsage(conversationId: number, message: ChatMessage): void {
    if (!this.isAssistantMessage(message)) return;
    if (message.context_breakdown) {
      this.deps.chatPanel.setContextUsage(
        conversationId,
        message.context_breakdown,
      );
      return;
    }
    // Keep the existing bar for intermediate tool-use loops, but clear stale
    // usage once an assistant turn completes without breakdown metadata.
    if (message.tool_calls.length === 0) {
      this.deps.chatPanel.clearContextUsage(conversationId);
    }
  }

  private isAssistantMessage(message: ChatMessage): boolean {
    return (
      typeof message.sender === "object" &&
      message.sender !== null &&
      "Assistant" in message.sender
    );
  }
}
