import {
  elementScroll,
  observeElementOffset,
  observeElementRect,
  Virtualizer,
} from "@tanstack/virtual-core";
import type {
  ChatEvent,
  ChatMessage,
  ContextBreakdown,
  ImageAttachment,
  TaskList,
  ToolExecutionResult,
  ToolRequestType,
} from "@tyde/protocol";
import {
  type BackendKind,
  cancelConversation,
  getSettings,
  listModels,
  listProfiles,
  sendMessage,
} from "./bridge";
import {
  addFilesFromDataTransfer as inputAddFilesFromDataTransfer,
  addImageFile as inputAddImageFile,
  hasFileDataTransfer as inputHasFileDataTransfer,
  renderThumbnails as inputRenderThumbnails,
  openLightbox,
} from "./chat/input";
import { InputHistory } from "./chat/input_handler";
import {
  copyToClipboard,
  createMessageElement,
  createSystemMessageElement,
  refreshRelativeTimes,
  resolveModelLabel,
  setRelativeTimeElement,
} from "./chat/message_renderer";
import {
  createSessionSettings,
  type SessionSettingsHandle,
} from "./chat/session_settings";
import * as stream from "./chat/stream";
import {
  createToolState,
  resetToolState,
  type ToolState,
  createPendingToolCards as toolCreatePendingCards,
  handleToolCompleted as toolHandleCompleted,
  handleToolRequest as toolHandleRequest,
} from "./chat/tools";
import { formatShortcut } from "./keyboard";
import type { NotificationManager } from "./notifications";
import { logTabPerf, perfNow } from "./perf_debug";
import { TaskPanel } from "./tasks";

interface QueuedMessage {
  text: string;
  images?: ImageAttachment[];
}

type StoredMessage =
  | {
      kind: "chat";
      key: number;
      message: ChatMessage;
      element: HTMLElement | null;
    }
  | {
      kind: "system";
      key: number;
      text: string;
      style: "system" | "warning" | "error";
      element: HTMLElement | null;
    }
  | { kind: "streaming"; key: number; element: HTMLElement }
  | { kind: "retry"; key: number; element: HTMLElement };

let nextMessageKey = 1;

interface ConversationView {
  wrapper: HTMLElement;
  container: HTMLDivElement;
  chatWrapper: HTMLElement;
  inputArea: HTMLElement;
  textarea: HTMLTextAreaElement;
  sendBtn: HTMLButtonElement;
  cancelBtn: HTMLButtonElement;
  thumbnailRow: HTMLElement;
  pendingImages: ImageAttachment[];
  typingIndicator: HTMLElement;
  scrollToBottomBtn: HTMLElement;
  taskPanel: TaskPanel;
  streamState: stream.StreamState;
  toolState: ToolState;
  sessionSettings: SessionSettingsHandle;
  conversationId: number;
  queuedMessages: QueuedMessage[];
  queueIndicator: HTMLElement;
  pendingSteer: QueuedMessage | null;
  disconnected: boolean;
  userScrolledUp: boolean;
  programmaticScroll: boolean;
  retryCard: HTMLElement | null;
  retryCountdownTimer: number | null;
  loadingOverlay: HTMLElement;
  messages: StoredMessage[];
  virtualizer: Virtualizer<HTMLElement, HTMLElement>;
  teardownVirtualizer: (() => void) | null;
  renderingMessages: boolean;
  renderDirty: boolean;
}

interface ParsedLinkedFileTarget {
  path: string;
  line?: number;
}

function createMessageElementForStored(stored: StoredMessage): HTMLElement {
  switch (stored.kind) {
    case "chat":
      return createMessageElement(
        stored.message,
        resolveModelLabel,
        copyToClipboard,
        openLightbox,
        setRelativeTimeElement,
      );
    case "system":
      return createSystemMessageElement(stored.text, stored.style);
    case "streaming":
      return stored.element;
    case "retry":
      return stored.element;
  }
}

function getOrCreateElement(stored: StoredMessage): HTMLElement {
  if (stored.kind === "streaming" || stored.kind === "retry") {
    return stored.element;
  }
  if (stored.element) return stored.element;
  const el = createMessageElementForStored(stored);
  stored.element = el;
  return el;
}

function updateVirtualizerCount(view: ConversationView): void {
  view.virtualizer.setOptions({
    ...view.virtualizer.options,
    count: view.messages.length,
  });
  renderVisibleMessages(view);
}

function renderVisibleMessages(view: ConversationView): void {
  if (view.renderingMessages) {
    view.renderDirty = true;
    return;
  }
  view.renderingMessages = true;
  view.renderDirty = false;

  const { virtualizer, chatWrapper, messages } = view;
  virtualizer._willUpdate();

  chatWrapper.style.height = `${virtualizer.getTotalSize()}px`;

  if (messages.length === 0) {
    for (const child of Array.from(chatWrapper.children)) {
      child.remove();
    }
    view.renderingMessages = false;
    return;
  }

  const virtualItems = virtualizer.getVirtualItems();

  // Build set of indices that should be visible
  const visibleIndices = new Set<number>();
  for (const item of virtualItems) {
    visibleIndices.add(item.index);
  }

  // Build map of currently mounted elements by index
  const mountedElements = new Map<number, HTMLElement>();
  let removedAny = false;
  for (const child of Array.from(chatWrapper.children)) {
    const el = child as HTMLElement;
    const idx = el.dataset.index;
    if (idx === undefined || !visibleIndices.has(Number(idx))) {
      el.remove();
      removedAny = true;
    } else {
      mountedElements.set(Number(idx), el);
    }
  }
  // Clean up ResizeObserver entries for removed (disconnected) elements.
  if (removedAny) {
    virtualizer.measureElement(null as unknown as HTMLElement);
  }

  // Add new elements and update positions
  for (const item of virtualItems) {
    let el = mountedElements.get(item.index);
    if (!el) {
      el = getOrCreateElement(messages[item.index]);
      el.dataset.index = String(item.index);
      el.style.position = "absolute";
      el.style.top = "0";
      el.style.left = "0";
      el.style.right = "0";
      chatWrapper.appendChild(el);
    }
    el.style.transform = `translateY(${item.start}px)`;
    // Measure all visible elements — not just newly mounted ones.
    // Retained elements may have been mutated while offscreen (e.g. tool
    // card content updates) and their cached heights could be stale.
    virtualizer.measureElement(el);
  }

  // Update all positions after measurement (sizes may have changed)
  virtualizer._willUpdate();
  for (const item of virtualizer.getVirtualItems()) {
    const el = chatWrapper.querySelector(
      `[data-index="${item.index}"]`,
    ) as HTMLElement | null;
    if (el) el.style.transform = `translateY(${item.start}px)`;
  }
  chatWrapper.style.height = `${virtualizer.getTotalSize()}px`;

  view.renderingMessages = false;

  // If onChange fired during this render, do one more pass
  if (view.renderDirty) {
    renderVisibleMessages(view);
  }
}

const ANSI_OSC_RE = /\u001b\][^\u0007]*(?:\u0007|\u001b\\)/g;
const ANSI_CSI_RE = /\u001b\[[0-9;?]*[ -/]*[@-~]/g;

function stripAnsiSequences(value: string): string {
  return value
    .replace(ANSI_OSC_RE, "")
    .replace(ANSI_CSI_RE, "")
    .replace(/\u001b/g, "");
}

export class ChatPanel {
  private container: HTMLElement;
  private welcomeEl: HTMLElement;
  private views: Map<number, ConversationView> = new Map();
  private detachedViews = new Set<number>();
  private conversationBackendKinds = new Map<number, BackendKind>();
  private typingByConversation = new Map<number, boolean>();
  private activeConversationId: number | null = null;
  private inputHistory = new InputHistory();
  private spawnLoadingOverlay: HTMLElement | null = null;

  notificationManager: NotificationManager | null = null;
  private relativeTimeTicker: number | null = null;

  onViewDiff:
    | ((filePath: string, before: string, after: string) => void)
    | null = null;
  onNewChat: (() => void) | null = null;
  onUserMessageSent:
    | ((
        conversationId: number,
        text: string,
        images?: ImageAttachment[],
      ) => void)
    | null = null;
  onOpenFileLink: ((filePath: string, oneBasedLine?: number) => void) | null =
    null;
  onSlashCommand: ((command: string) => boolean) | null = null;

  constructor(container: HTMLElement) {
    this.container = container;

    this.welcomeEl = document.createElement("div");
    this.welcomeEl.className = "chat-welcome-wrapper";
    this.welcomeEl.style.display = "none";
    this.welcomeEl.style.flex = "1";
    this.welcomeEl.style.overflow = "auto";
    this.container.appendChild(this.welcomeEl);

    this.setupCopyDelegation();
    this.setupLinkDelegation();
    this.setupViewDiffDelegation();
    this.ensureRelativeTimeTicker();
    refreshRelativeTimes(this.container);
  }

  // --- Per-conversation view management ---

  private getOrCreateView(conversationId: number): ConversationView {
    const existing = this.views.get(conversationId);
    if (existing) return existing;

    const wrapper = document.createElement("div");
    wrapper.className = "conversation-wrapper";
    wrapper.style.display = "none";
    wrapper.style.flexDirection = "column";
    wrapper.style.flex = "1";
    wrapper.style.minHeight = "0";
    wrapper.style.position = "relative";

    const container = document.createElement("div") as HTMLDivElement;
    container.className = "chat-container";
    container.dataset.testid = "chat-container";
    container.setAttribute("role", "log");
    container.setAttribute("aria-live", "polite");

    const chatWrapper = document.createElement("div");
    chatWrapper.className = "chat-message-wrapper";
    container.appendChild(chatWrapper);

    const taskBarEl = document.createElement("div");
    const taskPanel = new TaskPanel(taskBarEl);

    const typingIndicator = document.createElement("div");
    typingIndicator.className = "typing-indicator hidden";
    typingIndicator.dataset.testid = "typing-indicator";
    typingIndicator.setAttribute("aria-live", "assertive");
    typingIndicator.innerHTML =
      '<div class="typing-dot"></div><div class="typing-dot"></div><div class="typing-dot"></div>';

    const inputArea = document.createElement("div");
    inputArea.className = "input-area";
    inputArea.dataset.testid = "input-area";

    const textarea = document.createElement("textarea");
    textarea.rows = 1;
    textarea.placeholder = "Type a message...";
    textarea.setAttribute("role", "textbox");
    textarea.setAttribute("aria-label", "Message input");

    const thumbnailRow = document.createElement("div");
    thumbnailRow.className = "image-thumbnail-row hidden";

    const fileInput = document.createElement("input") as HTMLInputElement;
    fileInput.type = "file";
    fileInput.accept =
      "image/png,image/jpeg,image/jpg,image/gif,image/webp,image/svg+xml";
    fileInput.multiple = true;
    fileInput.style.display = "none";

    const btnGroup = document.createElement("div");
    btnGroup.className = "input-buttons";

    const attachBtn = document.createElement("button") as HTMLButtonElement;
    attachBtn.className = "attach-btn";
    attachBtn.textContent = "\u{1F4CE}";
    attachBtn.setAttribute("aria-label", "Attach image");

    const sendBtn = document.createElement("button") as HTMLButtonElement;
    sendBtn.className = "send-btn";
    sendBtn.dataset.testid = "send-btn";
    sendBtn.textContent = "Send";
    sendBtn.setAttribute("aria-label", "Send message");

    const cancelBtn = document.createElement("button") as HTMLButtonElement;
    cancelBtn.className = "cancel-btn";
    cancelBtn.dataset.testid = "cancel-btn";
    cancelBtn.textContent = "Interrupt";
    cancelBtn.disabled = true;
    cancelBtn.setAttribute("aria-label", "Interrupt generation");

    btnGroup.appendChild(attachBtn);
    btnGroup.appendChild(sendBtn);
    btnGroup.appendChild(cancelBtn);
    inputArea.appendChild(textarea);
    inputArea.appendChild(thumbnailRow);
    inputArea.appendChild(fileInput);
    inputArea.appendChild(btnGroup);

    const scrollToBottomBtn = document.createElement("button");
    scrollToBottomBtn.className = "scroll-to-bottom hidden";
    scrollToBottomBtn.dataset.testid = "scroll-to-bottom";
    scrollToBottomBtn.textContent = "\u2193";

    const queueIndicator = document.createElement("div");
    queueIndicator.className = "queue-indicator hidden";
    queueIndicator.dataset.testid = "queue-indicator";

    const backendKind =
      this.conversationBackendKinds.get(conversationId) ?? "tycode";
    const sessionSettings = createSessionSettings(conversationId, backendKind);

    const loadingOverlay = document.createElement("div");
    loadingOverlay.className = "chat-loading-overlay hidden";
    loadingOverlay.dataset.testid = "chat-loading-overlay";
    const loadingSpinner = document.createElement("div");
    loadingSpinner.className = "loading-spinner";
    loadingOverlay.appendChild(loadingSpinner);

    wrapper.appendChild(taskBarEl);
    wrapper.appendChild(container);
    wrapper.appendChild(loadingOverlay);
    wrapper.appendChild(typingIndicator);
    wrapper.appendChild(queueIndicator);
    wrapper.appendChild(inputArea);
    wrapper.appendChild(sessionSettings.element);
    wrapper.appendChild(scrollToBottomBtn);

    const messages: StoredMessage[] = [];

    const virtualizer = new Virtualizer<HTMLElement, HTMLElement>({
      count: 0,
      getScrollElement: () => container,
      estimateSize: () => 120,
      getItemKey: (index) => view.messages[index].key,
      overscan: 5,
      gap: 6,
      paddingEnd: 8,
      scrollToFn: elementScroll,
      observeElementRect,
      observeElementOffset,
      onChange: () => renderVisibleMessages(view),
    });

    const view: ConversationView = {
      wrapper,
      container,
      chatWrapper,
      inputArea,
      textarea,
      sendBtn,
      cancelBtn,
      thumbnailRow,
      pendingImages: [],
      typingIndicator,
      scrollToBottomBtn,
      taskPanel,
      streamState: stream.createStreamState(),
      toolState: createToolState(),
      sessionSettings,
      conversationId,
      queuedMessages: [],
      queueIndicator,
      pendingSteer: null,
      disconnected: false,
      userScrolledUp: false,
      programmaticScroll: false,
      retryCard: null,
      retryCountdownTimer: null,
      loadingOverlay,
      messages,
      virtualizer,
      teardownVirtualizer: null,
      renderingMessages: false,
      renderDirty: false,
    };

    view.teardownVirtualizer = virtualizer._didMount();
    virtualizer._willUpdate();

    this.wireViewEvents(view, conversationId, cancelBtn, fileInput, attachBtn);
    this.container.appendChild(wrapper);
    this.views.set(conversationId, view);
    getSettings(conversationId).catch((err) =>
      console.error("Failed to get settings for conversation:", err),
    );
    if (backendKind === "tycode") {
      listProfiles(conversationId).catch((err) =>
        console.error("Failed to list profiles for conversation:", err),
      );
    } else if (
      backendKind === "codex" ||
      backendKind === "claude" ||
      backendKind === "kiro"
    ) {
      listModels(conversationId).catch((err) =>
        console.error("Failed to list models for conversation:", err),
      );
    }
    return view;
  }

  private wireViewEvents(
    view: ConversationView,
    conversationId: number,
    cancelBtn: HTMLButtonElement,
    fileInput: HTMLInputElement,
    attachBtn: HTMLButtonElement,
  ): void {
    const { textarea, sendBtn, container } = view;
    const notifyError = (msg: string) => {
      this.notificationManager?.error(msg);
    };
    const updateSend = () => this.updateViewSendButton(view);

    fileInput.addEventListener("change", () => {
      if (!fileInput.files) return;
      for (const file of fileInput.files) {
        inputAddImageFile(view, file, notifyError, updateSend);
      }
      fileInput.value = "";
    });
    attachBtn.addEventListener("click", () => fileInput.click());

    const doSend = async () => {
      const text = textarea.value.trim();
      if ((!text && view.pendingImages.length === 0) || view.disconnected)
        return;

      if (text.startsWith("/") && this.onSlashCommand) {
        const handled = this.onSlashCommand(text);
        if (handled) {
          textarea.value = "";
          textarea.style.height = "auto";
          return;
        }
      }

      const aiIsTyping = !view.typingIndicator.classList.contains("hidden");

      if (aiIsTyping) {
        const images =
          view.pendingImages.length > 0 ? [...view.pendingImages] : undefined;
        if (text) this.inputHistory.push(text);
        this.inputHistory.reset();
        view.queuedMessages.push({ text, images });
        textarea.value = "";
        textarea.style.height = "auto";
        view.pendingImages = [];
        inputRenderThumbnails(view, updateSend);
        updateSend();
        this.updateCancelButton(view);
        this.updateQueueIndicator(view);
        return;
      }

      const images =
        view.pendingImages.length > 0 ? [...view.pendingImages] : undefined;
      if (text) this.inputHistory.push(text);
      this.inputHistory.reset();

      textarea.value = "";
      textarea.style.height = "auto";
      view.pendingImages = [];
      inputRenderThumbnails(view, updateSend);
      updateSend();

      try {
        await sendMessage(conversationId, text, images);
        this.onUserMessageSent?.(conversationId, text, images);
      } catch (err: unknown) {
        const msg = err instanceof Error ? err.message : String(err);
        if (
          msg.includes("broken pipe") ||
          msg.includes("subprocess") ||
          msg.includes("not found")
        ) {
          view.disconnected = true;
          this.appendSystemMessage(
            view,
            "Backend process unavailable. Open a new tab to continue.",
            "error",
          );
          updateSend();
          return;
        }
        this.appendSystemMessage(view, msg, "error");
      }
    };

    sendBtn.addEventListener("click", doSend);

    cancelBtn.addEventListener("click", () => {
      const isSteer = cancelBtn.textContent === "Steer";
      if (isSteer) {
        const text = textarea.value.trim();
        if (!text && view.pendingImages.length === 0) return;
        const images =
          view.pendingImages.length > 0 ? [...view.pendingImages] : undefined;
        if (text) this.inputHistory.push(text);
        this.inputHistory.reset();
        view.pendingSteer = { text, images };
        textarea.value = "";
        textarea.style.height = "auto";
        view.pendingImages = [];
        inputRenderThumbnails(view, updateSend);
        updateSend();
        this.updateCancelButton(view);
      }
      cancelConversation(conversationId);
    });

    textarea.addEventListener("keydown", (e) => {
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        doSend();
        return;
      }
      if (e.key === "ArrowUp") {
        if (textarea.value !== "" && textarea.selectionStart !== 0) return;
        const entry = this.inputHistory.up(textarea.value);
        if (entry === null) return;
        e.preventDefault();
        textarea.value = entry;
        textarea.selectionStart = textarea.value.length;
        textarea.selectionEnd = textarea.value.length;
        return;
      }
      if (e.key === "ArrowDown") {
        if (!this.inputHistory.isBrowsing()) return;
        const entry = this.inputHistory.down();
        if (entry === null) return;
        e.preventDefault();
        textarea.value = entry;
        textarea.selectionStart = textarea.value.length;
        textarea.selectionEnd = textarea.value.length;
        return;
      }
    });

    textarea.addEventListener("input", () => {
      textarea.style.height = "auto";
      textarea.style.height = `${Math.min(textarea.scrollHeight, 120)}px`;
      textarea.style.overflowY =
        textarea.scrollHeight > 120 ? "auto" : "hidden";
      updateSend();
      this.updateCancelButton(view);
    });

    textarea.addEventListener("paste", (e) => {
      const items = e.clipboardData?.items;
      if (!items) return;
      for (const item of items) {
        if (!item.type.startsWith("image/")) continue;
        e.preventDefault();
        const file = item.getAsFile();
        if (file) inputAddImageFile(view, file, notifyError, updateSend);
      }
    });

    this.wireViewDragDrop(view, notifyError, updateSend);

    view.scrollToBottomBtn.addEventListener("click", () => {
      view.userScrolledUp = false;
      this.scrollToBottom(view);
      this.updateViewScrollButton(view);
    });

    container.addEventListener("scroll", () => {
      if (view.programmaticScroll) return;
      view.userScrolledUp = !this.isViewNearBottom(view);
      this.updateViewScrollButton(view);
    });
  }

  private wireViewDragDrop(
    view: ConversationView,
    notifyError: (msg: string) => void,
    updateSend: () => void,
  ): void {
    let dragDepth = 0;
    const { wrapper, inputArea } = view;

    wrapper.addEventListener("dragenter", (e: DragEvent) => {
      if (!inputHasFileDataTransfer(e.dataTransfer)) return;
      e.preventDefault();
      dragDepth += 1;
      inputArea.classList.add("drop-zone-active");
    });
    wrapper.addEventListener("dragover", (e: DragEvent) => {
      if (!inputHasFileDataTransfer(e.dataTransfer)) return;
      e.preventDefault();
      if (e.dataTransfer) e.dataTransfer.dropEffect = "copy";
      inputArea.classList.add("drop-zone-active");
    });
    wrapper.addEventListener("dragleave", (e: DragEvent) => {
      if (!inputHasFileDataTransfer(e.dataTransfer)) return;
      e.preventDefault();
      dragDepth = Math.max(0, dragDepth - 1);
      if (dragDepth === 0) inputArea.classList.remove("drop-zone-active");
    });
    wrapper.addEventListener("drop", (e: DragEvent) => {
      if (!inputHasFileDataTransfer(e.dataTransfer)) return;
      e.preventDefault();
      dragDepth = 0;
      inputArea.classList.remove("drop-zone-active");
      const result = inputAddFilesFromDataTransfer(
        view,
        e.dataTransfer,
        notifyError,
        updateSend,
      );
      if (result.hadFiles && result.added === 0) {
        this.notificationManager?.error("Only image files can be attached");
      }
    });
  }

  switchToConversation(conversationId: number): void {
    this.hideSpawnLoading();
    const switchStart = perfNow();
    let hidePrevMs = 0;
    let getViewMs = 0;
    let showViewMs = 0;
    let uiUpdateMs = 0;
    let relativeTimesMs = 0;

    if (this.activeConversationId !== null) {
      const hidePrevStart = perfNow();
      const prevView = this.views.get(this.activeConversationId);
      if (prevView && !this.detachedViews.has(this.activeConversationId!)) {
        if (prevView.retryCountdownTimer !== null) {
          clearTimeout(prevView.retryCountdownTimer);
          prevView.retryCountdownTimer = null;
        }
        prevView.wrapper.style.display = "none";
      }
      hidePrevMs = perfNow() - hidePrevStart;
    }

    this.welcomeEl.style.display = "none";
    this.container.classList.remove("chat-panel-welcome");

    const getViewStart = perfNow();
    const view = this.getOrCreateView(conversationId);
    getViewMs = perfNow() - getViewStart;
    this.activeConversationId = conversationId;
    const showViewStart = perfNow();
    view.wrapper.style.display = "flex";
    this.applyViewTypingVisible(
      view,
      this.typingByConversation.get(conversationId) === true,
      false,
    );
    showViewMs = perfNow() - showViewStart;
    if (!view.userScrolledUp) {
      requestAnimationFrame(() => {
        if (this.activeConversationId !== conversationId) return;
        this.scrollToBottom(view);
        this.updateViewScrollButton(view);
      });
    }

    const uiUpdateStart = perfNow();
    this.updateViewSendButton(view);
    this.updateViewScrollButton(view);
    this.updateCancelButton(view);
    uiUpdateMs = perfNow() - uiUpdateStart;
    const relativeTimesStart = perfNow();
    refreshRelativeTimes(view.container);
    relativeTimesMs = perfNow() - relativeTimesStart;

    logTabPerf("ChatPanel.switchToConversation", perfNow() - switchStart, {
      conversationId,
      hidePrevMs,
      getViewMs,
      showViewMs,
      uiUpdateMs,
      relativeTimesMs,
      userScrolledUp: view.userScrolledUp,
    });
  }

  removeConversation(conversationId: number): void {
    this.detachedViews.delete(conversationId);
    this.conversationBackendKinds.delete(conversationId);
    this.typingByConversation.delete(conversationId);
    const view = this.views.get(conversationId);
    if (!view) return;
    if (view.teardownVirtualizer) {
      view.teardownVirtualizer();
      view.teardownVirtualizer = null;
    }
    view.wrapper.remove();
    this.views.delete(conversationId);
    if (this.activeConversationId === conversationId) {
      this.activeConversationId = null;
    }
  }

  // --- Conversation event routing ---

  handleConversationEvent(conversationId: number, event: ChatEvent): void {
    const view = this.getOrCreateView(conversationId);
    if (event.kind === "StreamStart") {
      const agent = event.data.agent.trim().toLowerCase();
      if (agent === "codex") {
        this.setConversationBackendKind(conversationId, "codex");
      } else if (agent === "claude" || agent === "claude_code") {
        this.setConversationBackendKind(conversationId, "claude");
      } else if (agent === "kiro") {
        this.setConversationBackendKind(conversationId, "kiro");
      }
    }

    switch (event.kind) {
      case "StreamStart":
        this.handleStreamStart(
          view,
          event.data.agent,
          event.data.model ?? null,
        );
        break;
      case "StreamDelta":
        this.handleStreamDelta(view, event.data.text);
        break;
      case "StreamReasoningDelta":
        this.handleReasoningDelta(view, event.data.text);
        break;
      case "StreamEnd":
        this.handleStreamEnd(view, event.data.message);
        break;
      case "Error":
        if (view.streamState.currentBubble) {
          this.handleStreamInterruption(view, event.data);
        } else {
          this.appendSystemMessage(view, event.data, "error");
        }
        this.notificationManager?.notifyError(event.data);
        break;
      case "SubprocessStderr": {
        const line = stripAnsiSequences(event.data).trim();
        if (!line) break;
        const clipped =
          line.length > 300 ? `${line.slice(0, 300)}\u2026` : line;
        this.appendSystemMessage(view, `Backend stderr: ${clipped}`, "warning");
        break;
      }
      case "ToolRequest":
        this.handleToolRequest(
          view,
          event.data.tool_call_id,
          event.data.tool_name,
          event.data.tool_type,
        );
        if (
          event.data.tool_name === "ask_user_question" ||
          event.data.tool_name === "AskUserQuestion"
        ) {
          this.notificationManager?.notifyUserInputNeeded(
            "AI is waiting for your response",
          );
        }
        break;
      case "ToolExecutionCompleted":
        this.handleToolCompleted(
          view,
          event.data.tool_call_id,
          event.data.tool_name,
          event.data.tool_result,
          event.data.success,
        );
        break;
      case "MessageAdded":
        this.renderFullMessage(view, event.data);
        break;
      case "ConversationCleared":
        this.clearConversation(view);
        break;
      case "OperationCancelled":
        this.appendSystemMessage(view, event.data.message, "system");
        break;
      case "RetryAttempt":
        this.showRetryCard(
          view,
          event.data.attempt,
          event.data.max_retries,
          event.data.error,
          event.data.backoff_ms,
        );
        break;
      case "TypingStatusChanged":
        this.setConversationTypingVisible(view, event.data === true);
        break;
      case "SubprocessExit":
        view.disconnected = true;
        this.setConversationTypingVisible(view, false);
        if (view.streamState.currentBubble) {
          this.handleStreamInterruption(view, "Backend process exited");
        }
        this.appendSystemMessage(
          view,
          "Backend process exited. Open a new tab to continue.",
          "error",
        );
        this.updateViewSendButton(view);
        break;
      default:
        break;
    }
  }

  handleEvent(event: ChatEvent): void {
    if (this.activeConversationId !== null) {
      this.handleConversationEvent(this.activeConversationId, event);
    }
  }

  // --- Detach/reattach for docked conversations ---

  detachView(conversationId: number): HTMLElement | null {
    const view = this.views.get(conversationId);
    if (!view) return null;
    view.wrapper.remove();
    this.detachedViews.add(conversationId);
    view.wrapper.style.display = "flex";
    if (this.activeConversationId === conversationId) {
      this.activeConversationId = null;
    }
    this.setupDelegationOnWrapper(view.wrapper);
    return view.wrapper;
  }

  reattachView(conversationId: number): void {
    const view = this.views.get(conversationId);
    if (!view) return;
    this.detachedViews.delete(conversationId);
    this.container.appendChild(view.wrapper);
    view.wrapper.style.display = "none";
  }

  getConversationTitle(conversationId: number): string {
    return `Chat ${conversationId}`;
  }

  isDetached(conversationId: number): boolean {
    return this.detachedViews.has(conversationId);
  }

  private setupDelegationOnWrapper(wrapper: HTMLElement): void {
    wrapper.addEventListener("click", (e) => {
      const target = e.target as HTMLElement;

      if (target.classList.contains("copy-btn")) {
        const encoded = target.getAttribute("data-code");
        if (!encoded) return;
        const decoded = new TextDecoder().decode(
          Uint8Array.from(atob(encoded), (c) => c.charCodeAt(0)),
        );
        navigator.clipboard
          .writeText(decoded)
          .then(() => {
            target.textContent = "Copied!";
            setTimeout(() => {
              target.textContent = "Copy";
            }, 1500);
          })
          .catch((err) => {
            console.error("Failed to copy to clipboard:", err);
            target.textContent = "Failed";
            setTimeout(() => {
              target.textContent = "Copy";
            }, 1500);
          });
        return;
      }

      const anchor = target.closest("a");
      if (anchor instanceof HTMLAnchorElement) {
        const href = anchor.getAttribute("href");
        if (!href) return;

        const linkedFile = this.parseLinkedFileTarget(href);
        if (linkedFile && this.onOpenFileLink) {
          e.preventDefault();
          this.onOpenFileLink(linkedFile.path, linkedFile.line);
          return;
        }

        e.preventDefault();
        const tauriShell = (window as any).__TAURI__?.shell;
        if (tauriShell?.open) {
          tauriShell.open(href);
        } else {
          window.open(href, "_blank", "noopener,noreferrer");
        }
        return;
      }

      if (target.classList.contains("view-diff-btn")) {
        const diffId = target.getAttribute("data-diff-id");
        if (!diffId) return;
        for (const view of this.views.values()) {
          const data = view.toolState.diffData.get(diffId);
          if (!data) continue;
          this.onViewDiff?.(data.filePath, data.before, data.after);
          return;
        }
      }
    });
  }

  // --- Public accessors ---

  getConversationId(): number | null {
    return this.activeConversationId;
  }

  setConversationId(id: number): void {
    this.switchToConversation(id);
    const view = this.views.get(id);
    view?.textarea.focus();
  }

  showSpawnLoading(): void {
    this.hideSpawnLoading();
    if (this.activeConversationId !== null) {
      const prevView = this.views.get(this.activeConversationId);
      if (prevView && !this.detachedViews.has(this.activeConversationId)) {
        prevView.wrapper.style.display = "none";
      }
    }
    this.activeConversationId = null;
    this.welcomeEl.style.display = "none";
    this.container.classList.remove("chat-panel-welcome");

    const overlay = document.createElement("div");
    overlay.className = "chat-spawn-loading";
    overlay.innerHTML =
      '<div class="loading-spinner"></div><span>Starting agent\u2026</span>';
    this.container.appendChild(overlay);
    this.spawnLoadingOverlay = overlay;
  }

  hideSpawnLoading(): void {
    if (this.spawnLoadingOverlay) {
      this.spawnLoadingOverlay.remove();
      this.spawnLoadingOverlay = null;
    }
  }

  showSpawnError(message: string): void {
    this.hideSpawnLoading();
    const overlay = document.createElement("div");
    overlay.className = "chat-spawn-loading chat-spawn-error";
    overlay.textContent = message;
    this.container.appendChild(overlay);
    this.spawnLoadingOverlay = overlay;
  }

  // --- Chat clearing ---

  clearChat(): void {
    if (this.activeConversationId === null) return;
    const view = this.views.get(this.activeConversationId);
    if (!view) return;
    this.removeRetryCard(view);
    this.clearConversation(view);
  }

  private clearConversation(view: ConversationView): void {
    this.typingByConversation.set(view.conversationId, false);
    this.applyViewTypingVisible(view, false, false);
    if (view.retryCountdownTimer !== null) {
      clearTimeout(view.retryCountdownTimer);
      view.retryCountdownTimer = null;
    }
    view.retryCard = null;
    view.messages = [];
    updateVirtualizerCount(view);
    view.virtualizer.measure();
    view.chatWrapper.replaceChildren();
    stream.resetStreamState(view.streamState);
    resetToolState(view.toolState);
    view.queuedMessages = [];
    view.pendingSteer = null;
    this.updateQueueIndicator(view);
    view.userScrolledUp = false;
    view.taskPanel.clearState();
    this.updateViewScrollButton(view);
  }

  private appendSystemMessage(
    view: ConversationView,
    text: string,
    style: "system" | "warning" | "error",
  ): void {
    view.messages.push({
      kind: "system",
      key: nextMessageKey++,
      text,
      style,
      element: null,
    });
    updateVirtualizerCount(view);
    this.scrollToBottom(view);
  }

  // --- Stream handlers ---

  private handleStreamStart(
    view: ConversationView,
    agent: string,
    modelInfo: unknown,
  ): void {
    if (view.retryCard) {
      this.removeRetryCard(view);
      this.appendSystemMessage(view, "Reconnected", "system");
    }
    stream.handleStreamStart(
      view.streamState,
      (bubble) => {
        view.messages.push({
          kind: "streaming",
          key: nextMessageKey++,
          element: bubble,
        });
        updateVirtualizerCount(view);
      },
      agent,
      modelInfo,
      resolveModelLabel,
      () => this.scrollToBottom(view),
    );
  }

  private handleStreamDelta(view: ConversationView, text: string): void {
    stream.handleStreamDelta(view.streamState, text, () =>
      this.scrollToBottom(view),
    );
  }

  private handleReasoningDelta(view: ConversationView, text: string): void {
    stream.handleReasoningDelta(view.streamState, text, () =>
      this.scrollToBottom(view),
    );
  }

  private handleStreamInterruption(
    view: ConversationView,
    errorMessage: string,
  ): void {
    stream.handleStreamInterruption(view.streamState, errorMessage, () =>
      this.scrollToBottom(view),
    );
  }

  private handleStreamEnd(view: ConversationView, message: ChatMessage): void {
    const result = stream.handleStreamEnd(
      view.streamState,
      message,
      (msg: ChatMessage) =>
        createMessageElement(
          msg,
          resolveModelLabel,
          copyToClipboard,
          openLightbox,
          setRelativeTimeElement,
        ),
      resolveModelLabel,
      () => this.scrollToBottom(view),
    );
    // Backend controls typing status — do NOT call setViewTypingVisible here
    if (!result) {
      this.renderFullMessage(view, message);
    } else {
      // Replace the streaming entry with a finalized chat entry
      const streamIdx = view.messages.findIndex((m) => m.kind === "streaming");
      if (streamIdx !== -1) {
        const streamKey = view.messages[streamIdx].key;
        const rendered = view.streamState.lastRenderedBubble;
        if (rendered) {
          rendered.dataset.index = String(streamIdx);
          view.virtualizer.measureElement(rendered);
        }
        // Reposition all items — measureElement updated the size cache but
        // positions (translateY) are still stale from the pre-swap layout.
        renderVisibleMessages(view);
        view.messages[streamIdx] = {
          kind: "chat",
          key: streamKey,
          message,
          element: rendered,
        };
      }
      this.expandLastAssistantMessage(view);
      if (result.durationMs > 30_000) {
        this.notificationManager?.notifyTaskComplete("Response complete");
      }
    }

    if (message.tool_calls.length > 0 && view.streamState.lastRenderedBubble) {
      toolCreatePendingCards(
        view.toolState,
        message.tool_calls,
        view.streamState.lastRenderedBubble,
        () => this.scrollToBottom(view),
      );
    }
  }

  // --- Tool handlers ---

  private handleToolRequest(
    view: ConversationView,
    toolCallId: string,
    toolName: string,
    toolType: ToolRequestType,
  ): void {
    toolHandleRequest(
      view.toolState,
      toolCallId,
      toolName,
      toolType,
      view.streamState.currentBubble ?? view.streamState.lastRenderedBubble,
      view.chatWrapper,
      () => this.scrollToBottom(view),
    );
  }

  private handleToolCompleted(
    view: ConversationView,
    toolCallId: string,
    toolName: string,
    toolResult: ToolExecutionResult,
    success: boolean,
  ): void {
    const hadCard = toolHandleCompleted(
      view.toolState,
      toolCallId,
      toolName,
      toolResult,
      success,
      () => this.scrollToBottom(view),
    );
    if (hadCard) return;

    const errorDetail =
      toolResult.kind === "Error" ? toolResult.short_message : "";
    const label = success ? "completed" : "failed";
    const msg = errorDetail
      ? `Tool "${toolName}" ${label}: ${errorDetail}`
      : `Tool "${toolName}" ${label}`;
    const style = success ? ("system" as const) : ("error" as const);
    this.appendSystemMessage(view, msg, style);
  }

  // --- Message rendering ---

  private expandLastAssistantMessage(view: ConversationView): void {
    const { chatWrapper, virtualizer } = view;
    const msgs = chatWrapper.querySelectorAll(".message.assistant-message");
    if (msgs.length === 0) return;
    const last = msgs[msgs.length - 1] as HTMLElement;
    let changed = false;
    const lastTruncatable = last.querySelector(
      ".message-content > .truncatable.collapsed",
    );
    if (lastTruncatable) {
      lastTruncatable.classList.remove("collapsed");
      const btn = lastTruncatable.querySelector(".truncatable-toggle");
      if (btn) btn.textContent = "Show less";
      changed = true;
    }
    let prev: HTMLElement | null = null;
    if (msgs.length >= 2) {
      prev = msgs[msgs.length - 2] as HTMLElement;
      const prevTruncatable = prev.querySelector(
        ".message-content > .truncatable:not(.collapsed)",
      );
      if (prevTruncatable) {
        prevTruncatable.classList.add("collapsed");
        const btn = prevTruncatable.querySelector(".truncatable-toggle");
        if (btn) btn.textContent = "Show more";
        changed = true;
      }
    }
    if (changed) {
      if (last.dataset.index !== undefined) virtualizer.measureElement(last);
      if (prev?.dataset.index !== undefined) virtualizer.measureElement(prev);
      renderVisibleMessages(view);
    }
  }

  private renderFullMessage(
    view: ConversationView,
    message: ChatMessage,
  ): void {
    const el = createMessageElement(
      message,
      resolveModelLabel,
      copyToClipboard,
      openLightbox,
      setRelativeTimeElement,
    );
    const stored: StoredMessage = {
      kind: "chat",
      key: nextMessageKey++,
      message,
      element: el,
    };
    view.messages.push(stored);
    updateVirtualizerCount(view);
    view.streamState.lastRenderedBubble = el;
    this.expandLastAssistantMessage(view);
    if (message.context_breakdown) {
      view.taskPanel.setContextUsage(message.context_breakdown);
    }
    if (message.tool_calls.length > 0) {
      toolCreatePendingCards(view.toolState, message.tool_calls, el, () =>
        this.scrollToBottom(view),
      );
    }
    this.scrollToBottom(view);
  }

  // --- Retry card ---

  private showRetryCard(
    view: ConversationView,
    attempt: number,
    maxRetries: number,
    error: string,
    backoffMs: number,
  ): void {
    this.removeRetryCard(view);

    const card = document.createElement("div");
    card.className = "retry-card";

    const header = document.createElement("div");
    header.className = "retry-card-header";
    header.innerHTML = `<span class="retry-card-icon">\u23F3</span><span class="retry-card-title">Rate Limited</span><span class="retry-card-attempt">Attempt ${attempt} of ${maxRetries}</span>`;
    card.appendChild(header);

    const body = document.createElement("div");
    body.className = "retry-card-body";

    const errorEl = document.createElement("div");
    errorEl.className = "retry-card-error";
    errorEl.textContent =
      error.length > 150 ? `${error.slice(0, 150)}\u2026` : error;
    body.appendChild(errorEl);

    const countdownRow = document.createElement("div");
    countdownRow.className = "retry-card-countdown-row";

    const countdownText = document.createElement("span");
    countdownText.className = "retry-card-countdown-text";
    body.appendChild(countdownRow);

    const barContainer = document.createElement("div");
    barContainer.className = "retry-card-bar";
    const barFill = document.createElement("div");
    barFill.className = "retry-card-bar-fill";
    barContainer.appendChild(barFill);
    body.appendChild(barContainer);

    countdownRow.appendChild(countdownText);

    const hint = document.createElement("div");
    hint.className = "retry-card-hint";
    hint.textContent = "Consider reducing request frequency";
    body.appendChild(hint);

    card.appendChild(body);

    const actions = document.createElement("div");
    actions.className = "retry-card-actions";
    const cancelBtn = document.createElement("button");
    cancelBtn.className = "retry-card-cancel";
    cancelBtn.textContent = "Cancel";
    cancelBtn.addEventListener("click", () => {
      if (this.activeConversationId !== null) {
        cancelConversation(this.activeConversationId);
      }
      this.removeRetryCard(view);
    });
    actions.appendChild(cancelBtn);
    card.appendChild(actions);

    view.retryCard = card;
    view.messages.push({
      kind: "retry",
      key: nextMessageKey++,
      element: card,
    });
    updateVirtualizerCount(view);
    this.scrollToBottom(view);

    const startTime = Date.now();
    const tick = () => {
      const elapsed = Date.now() - startTime;
      const remaining = Math.max(0, backoffMs - elapsed);
      const pct = Math.max(0, remaining / backoffMs) * 100;
      countdownText.textContent = `Retrying in ${Math.ceil(remaining / 1000)}s...`;
      barFill.style.width = `${pct}%`;
      if (remaining > 0) {
        view.retryCountdownTimer = window.setTimeout(tick, 100);
      }
    };
    tick();
  }

  private removeRetryCard(view?: ConversationView): void {
    const target =
      view ??
      (this.activeConversationId !== null
        ? this.views.get(this.activeConversationId)
        : undefined);
    if (!target) return;
    if (target.retryCountdownTimer !== null) {
      clearTimeout(target.retryCountdownTimer);
      target.retryCountdownTimer = null;
    }
    if (target.retryCard) {
      const idx = target.messages.findIndex((m) => m.kind === "retry");
      if (idx !== -1) {
        target.messages.splice(idx, 1);
        // Mid-list removal shifts indices. Clear data-index on all mounted
        // elements so renderVisibleMessages treats them as stale and
        // re-mounts with correct indices.
        for (const child of Array.from(target.chatWrapper.children)) {
          delete (child as HTMLElement).dataset.index;
        }
        updateVirtualizerCount(target);
        target.virtualizer.measure();
      }
      target.retryCard = null;
    }
  }

  // --- Queue management ---

  restoreConversationTypingState(
    conversationId: number,
    visible: boolean,
  ): void {
    this.typingByConversation.set(conversationId, visible);
    const view = this.views.get(conversationId);
    if (!view) return;
    this.applyViewTypingVisible(view, visible, false);
  }

  private setConversationTypingVisible(
    view: ConversationView,
    visible: boolean,
  ): void {
    this.typingByConversation.set(view.conversationId, visible);
    this.applyViewTypingVisible(view, visible, true);
  }

  private applyViewTypingVisible(
    view: ConversationView,
    visible: boolean,
    drainQueueOnStop: boolean,
  ): void {
    const wasVisible = !view.typingIndicator.classList.contains("hidden");

    if (visible) {
      view.typingIndicator.classList.remove("hidden");
    } else {
      view.typingIndicator.classList.add("hidden");

      // Only drain on actual visible→hidden transition to prevent double drains.
      if (drainQueueOnStop && wasVisible) {
        if (view.pendingSteer) {
          // Steer takes priority — send the steered message, skip normal drain
          const steer = view.pendingSteer;
          view.pendingSteer = null;
          this.drainQueuedMessage(view, steer.text, steer.images);
        } else if (view.queuedMessages.length > 0) {
          const next = view.queuedMessages.shift()!;
          this.updateQueueIndicator(view);
          this.drainQueuedMessage(view, next.text, next.images);
        }
      }
    }

    this.updateCancelButton(view);
    this.scrollToBottom(view);
  }

  private drainQueuedMessage(
    view: ConversationView,
    text: string,
    images?: ImageAttachment[],
  ): void {
    sendMessage(view.conversationId, text, images)
      .then(() => {
        this.onUserMessageSent?.(view.conversationId, text, images);
      })
      .catch((err: unknown) => {
        const msg = err instanceof Error ? err.message : String(err);
        if (
          msg.includes("broken pipe") ||
          msg.includes("subprocess") ||
          msg.includes("not found")
        ) {
          view.disconnected = true;
          this.appendSystemMessage(
            view,
            "Backend process unavailable. Open a new tab to continue.",
            "error",
          );
          this.updateViewSendButton(view);
          return;
        }
        this.appendSystemMessage(view, msg, "error");
      });
  }

  private updateCancelButton(view: ConversationView): void {
    const isTyping = !view.typingIndicator.classList.contains("hidden");
    const hasText =
      view.textarea.value.trim().length > 0 || view.pendingImages.length > 0;

    if (!isTyping) {
      view.cancelBtn.disabled = true;
      view.cancelBtn.textContent = "Interrupt";
      return;
    }

    view.cancelBtn.disabled = false;
    if (hasText) {
      view.cancelBtn.textContent = "Steer";
    } else {
      view.cancelBtn.textContent = "Interrupt";
    }
  }

  private steerFromQueue(view: ConversationView, index: number): void {
    const item = view.queuedMessages.splice(index, 1)[0];
    view.pendingSteer = item;
    this.updateQueueIndicator(view);
    cancelConversation(view.conversationId);
  }

  private updateQueueIndicator(view: ConversationView): void {
    const { queuedMessages, queueIndicator } = view;
    if (queuedMessages.length === 0) {
      queueIndicator.classList.add("hidden");
      queueIndicator.innerHTML = "";
      return;
    }
    queueIndicator.classList.remove("hidden");
    queueIndicator.innerHTML = "";

    for (let i = 0; i < queuedMessages.length; i++) {
      const item = queuedMessages[i];
      const row = document.createElement("div");
      row.className = "queue-item";
      row.dataset.testid = "queue-item";

      const steerBtn = document.createElement("button");
      steerBtn.className = "queue-item-steer";
      steerBtn.dataset.testid = "queue-item-steer";
      steerBtn.textContent = "\u2191";
      steerBtn.setAttribute("aria-label", "Send this message now");
      steerBtn.addEventListener("click", () => this.steerFromQueue(view, i));

      const textEl = document.createElement("span");
      textEl.className = "queue-item-text";
      textEl.dataset.testid = "queue-item-text";
      textEl.textContent = item.text;

      const removeBtn = document.createElement("button");
      removeBtn.className = "queue-item-remove";
      removeBtn.dataset.testid = "queue-item-remove";
      removeBtn.textContent = "\u00D7";
      removeBtn.setAttribute("aria-label", "Remove queued message");
      removeBtn.addEventListener("click", () => {
        view.queuedMessages.splice(i, 1);
        this.updateQueueIndicator(view);
      });

      row.appendChild(textEl);
      row.appendChild(steerBtn);
      row.appendChild(removeBtn);
      queueIndicator.appendChild(row);
    }
  }

  // Event delegation on this.container so events bubble from any conversation wrapper
  private setupLinkDelegation(): void {
    this.container.addEventListener("click", (e) => {
      const target = e.target as HTMLElement | null;
      if (!target) return;

      const anchor = target.closest("a");
      if (!(anchor instanceof HTMLAnchorElement)) return;

      const href = anchor.getAttribute("href");
      if (!href) return;

      const linkedFile = this.parseLinkedFileTarget(href);
      if (linkedFile && this.onOpenFileLink) {
        e.preventDefault();
        this.onOpenFileLink(linkedFile.path, linkedFile.line);
        return;
      }

      e.preventDefault();
      const tauriShell = (window as any).__TAURI__?.shell;
      if (tauriShell?.open) {
        tauriShell.open(href);
      } else {
        window.open(href, "_blank", "noopener,noreferrer");
      }
    });
  }

  private parseLinkedFileTarget(
    rawHref: string,
  ): ParsedLinkedFileTarget | null {
    const href = rawHref.trim();
    if (!href || href.startsWith("#")) return null;
    if (/^(https?|mailto):/i.test(href)) return null;

    let pathPart = href;
    const queryIdx = pathPart.indexOf("?");
    if (queryIdx !== -1) {
      pathPart = pathPart.slice(0, queryIdx);
    }

    let hashLine: number | undefined;
    const hashIdx = pathPart.indexOf("#");
    if (hashIdx !== -1) {
      hashLine = this.parseLinkedFileHashLine(pathPart.slice(hashIdx + 1));
      pathPart = pathPart.slice(0, hashIdx);
    }

    let suffixLine: number | undefined;
    const suffixMatch = pathPart.match(/:(\d+)(?::\d+)?$/);
    if (suffixMatch && suffixMatch.index !== undefined) {
      const parsed = Number.parseInt(suffixMatch[1], 10);
      if (Number.isFinite(parsed) && parsed > 0) {
        suffixLine = parsed;
      }
      pathPart = pathPart.slice(0, suffixMatch.index);
    }

    let decodedPath: string;
    try {
      decodedPath = decodeURIComponent(pathPart);
    } catch {
      return null;
    }
    if (!this.isLocalFilePath(decodedPath)) return null;

    const line = suffixLine ?? hashLine;
    if (line === undefined && !this.hasFileLikeBasename(decodedPath))
      return null;
    if (line !== undefined) {
      return { path: decodedPath, line };
    }
    return { path: decodedPath };
  }

  private parseLinkedFileHashLine(rawHash: string): number | undefined {
    const hash = rawHash.trim();
    if (!hash) return undefined;
    const match = hash.match(/^L(\d+)(?:C\d+)?$/i) ?? hash.match(/^(\d+)$/);
    if (!match) return undefined;
    const parsed = Number.parseInt(match[1], 10);
    if (!Number.isFinite(parsed) || parsed <= 0) return undefined;
    return parsed;
  }

  private isLocalFilePath(path: string): boolean {
    if (path.startsWith("//")) return false;
    if (path.startsWith("/")) return true;
    if (path.startsWith("./") || path.startsWith("../")) return true;
    return /^[A-Za-z]:[\\/]/.test(path);
  }

  private hasFileLikeBasename(path: string): boolean {
    const slashIdx = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
    const base = slashIdx >= 0 ? path.slice(slashIdx + 1) : path;
    if (!base || base === "." || base === "..") return false;
    return base.includes(".");
  }

  private setupViewDiffDelegation(): void {
    this.container.addEventListener("click", (e) => {
      const target = e.target as HTMLElement;
      if (!target.classList.contains("view-diff-btn")) return;
      const diffId = target.getAttribute("data-diff-id");
      if (!diffId) return;
      for (const view of this.views.values()) {
        const data = view.toolState.diffData.get(diffId);
        if (!data) continue;
        this.onViewDiff?.(data.filePath, data.before, data.after);
        return;
      }
    });
  }

  private setupCopyDelegation(): void {
    this.container.addEventListener("click", async (e) => {
      const target = e.target as HTMLElement;
      if (!target.classList.contains("copy-btn")) return;
      const encoded = target.getAttribute("data-code");
      if (!encoded) return;
      try {
        const decoded = new TextDecoder().decode(
          Uint8Array.from(atob(encoded), (c) => c.charCodeAt(0)),
        );
        await navigator.clipboard.writeText(decoded);
        target.textContent = "Copied!";
        setTimeout(() => {
          target.textContent = "Copy";
        }, 1500);
      } catch (e) {
        console.error("Copy failed:", e);
        target.textContent = "Failed";
        setTimeout(() => {
          target.textContent = "Copy";
        }, 1500);
      }
    });
  }

  // --- Lifecycle ---

  clear(): void {
    this.hideSpawnLoading();
    if (this.activeConversationId !== null) {
      const view = this.views.get(this.activeConversationId);
      if (view) view.wrapper.style.display = "none";
    }
    this.activeConversationId = null;
    this.welcomeEl.style.display = "none";
    this.container.classList.remove("chat-panel-welcome");
  }

  showWelcome(): void {
    if (this.activeConversationId !== null) {
      const view = this.views.get(this.activeConversationId);
      if (view) view.wrapper.style.display = "none";
    }
    this.activeConversationId = null;
    this.container.classList.add("chat-panel-welcome");

    this.welcomeEl.innerHTML = "";
    const welcome = document.createElement("div");
    welcome.className = "welcome-screen";
    welcome.dataset.testid = "welcome-screen";
    welcome.innerHTML = `
      <div class="welcome-logo"><img src="/tycode-tiger.png" alt="Tycode tiger" class="welcome-logo-img" /></div>
      <h1 class="welcome-title">Tyde</h1>
      <p class="welcome-subtitle">Coding Agent Studio</p>
      <div class="welcome-actions">
        <button id="welcome-new-chat" data-testid="welcome-new-chat" class="welcome-btn">New Chat Tab</button>
      </div>
      <div class="welcome-shortcuts">
        <div class="shortcut-row"><kbd>${formatShortcut("Ctrl+K")}</kbd><span>Command palette</span></div>
        <div class="shortcut-row"><kbd>Enter</kbd><span>Send message</span></div>
        <div class="shortcut-row"><kbd>Shift+Enter</kbd><span>New line</span></div>
        <div class="shortcut-row"><kbd>Escape</kbd><span>Cancel generation</span></div>
        <div class="shortcut-row"><kbd>${formatShortcut("Ctrl+/")}</kbd><span>All shortcuts</span></div>
      </div>
    `;
    this.welcomeEl.appendChild(welcome);
    this.welcomeEl.style.display = "block";

    const newChatBtn = welcome.querySelector("#welcome-new-chat");
    if (newChatBtn) {
      newChatBtn.addEventListener("click", () => {
        this.onNewChat?.();
      });
    }
  }

  updateTaskList(conversationId: number, taskList: TaskList): void {
    const view = this.views.get(conversationId);
    if (!view) return;
    view.taskPanel.update(taskList);
  }

  setContextUsage(conversationId: number, breakdown: ContextBreakdown): void {
    const view = this.views.get(conversationId);
    if (!view) return;
    view.taskPanel.setContextUsage(breakdown);
  }

  clearContextUsage(conversationId: number): void {
    const view = this.views.get(conversationId);
    if (!view) return;
    view.taskPanel.clearContextUsage();
  }

  handleSettingsUpdate(conversationId: number, data: unknown): void {
    const view = this.views.get(conversationId);
    if (!view) return;
    view.sessionSettings.updateSettings(data);
  }

  handleProfilesList(conversationId: number, data: unknown): void {
    const view = this.views.get(conversationId);
    if (!view) return;
    view.sessionSettings.updateProfiles(
      data as { profiles: string[]; active_profile?: string },
    );
  }

  handleModelsList(conversationId: number, data: unknown): void {
    const view = this.views.get(conversationId);
    if (!view) return;
    view.sessionSettings.updateModels(
      data as {
        models: Array<{ id: string; displayName: string; isDefault: boolean }>;
      },
    );
  }

  setConversationBackendKind(
    conversationId: number,
    backendKind: BackendKind,
  ): void {
    this.conversationBackendKinds.set(conversationId, backendKind);
    const view = this.views.get(conversationId);
    if (!view) return;
    view.sessionSettings.setBackendKind(backendKind);
  }

  toggleActiveTaskBar(): void {
    if (this.activeConversationId === null) return;
    const view = this.views.get(this.activeConversationId);
    if (view) view.taskPanel.toggle();
  }

  setConnected(): void {
    if (this.activeConversationId === null) return;
    const view = this.views.get(this.activeConversationId);
    if (!view) return;
    view.disconnected = false;
    this.updateViewSendButton(view);
  }

  setHistoryLoading(conversationId: number, loading: boolean): void {
    const view = this.views.get(conversationId);
    if (!view) return;
    if (loading) {
      view.loadingOverlay.classList.remove("hidden");
    } else {
      view.loadingOverlay.classList.add("hidden");
    }
  }

  focusInput(): void {
    if (this.activeConversationId === null) return;
    const view = this.views.get(this.activeConversationId);
    view?.textarea.focus();
  }

  isTyping(): boolean {
    if (this.activeConversationId === null) return false;
    const view = this.views.get(this.activeConversationId);
    if (!view) return false;
    return !view.typingIndicator.classList.contains("hidden");
  }

  isStreaming(): boolean {
    if (this.activeConversationId === null) return false;
    const view = this.views.get(this.activeConversationId);
    if (!view) return false;
    return view.streamState.currentBubble !== null;
  }

  // --- Scroll ---

  private isViewNearBottom(view: ConversationView, threshold = 50): boolean {
    return (
      view.container.scrollHeight -
        view.container.scrollTop -
        view.container.clientHeight <
      threshold
    );
  }

  private scrollToBottom(view: ConversationView): void {
    if (view.userScrolledUp && !this.isViewNearBottom(view)) return;
    view.userScrolledUp = false;
    view.programmaticScroll = true;
    view.container.scrollTop = view.container.scrollHeight;
    requestAnimationFrame(() => {
      view.container.scrollTop = view.container.scrollHeight;
      view.programmaticScroll = false;
    });
  }

  private ensureRelativeTimeTicker(): void {
    if (this.relativeTimeTicker !== null) return;
    this.relativeTimeTicker = window.setInterval(() => {
      refreshRelativeTimes(this.container);
    }, 30_000);
  }

  private updateViewSendButton(view: ConversationView): void {
    view.sendBtn.disabled =
      view.disconnected ||
      (!view.textarea.value.trim() && view.pendingImages.length === 0);
  }

  private updateViewScrollButton(view: ConversationView): void {
    if (view.userScrolledUp) {
      view.scrollToBottomBtn.classList.remove("hidden");
    } else {
      view.scrollToBottomBtn.classList.add("hidden");
    }
  }
}
