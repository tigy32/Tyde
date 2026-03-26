import type { ChatMessage } from "@tyde/protocol";
import { escapeHtml, renderContent } from "../renderer";
import { normalizeReasoningText } from "./reasoning";

const STREAMING_CURSOR_ARTIFACT_RE =
  /\s*[\u2580-\u259f\u25a0\u25aa\u25ab\ufffd]+\s*$/u;

export interface StreamState {
  currentBubble: HTMLElement | null;
  currentContentEl: HTMLElement | null;
  currentReasoningEl: HTMLElement | null;
  streamingText: string;
  streamingReasoning: string;
  deltaBuffer: string;
  rafId: number | null;
  lastRenderedBubble: HTMLElement | null;
  streamStartTime: number;
  streamModelLabel: string | null;
}

export function createStreamState(): StreamState {
  return {
    currentBubble: null,
    currentContentEl: null,
    currentReasoningEl: null,
    streamingText: "",
    streamingReasoning: "",
    deltaBuffer: "",
    rafId: null,
    lastRenderedBubble: null,
    streamStartTime: 0,
    streamModelLabel: null,
  };
}

export function resetStreamState(state: StreamState): void {
  if (state.rafId !== null) {
    cancelAnimationFrame(state.rafId);
  }
  state.currentBubble = null;
  state.lastRenderedBubble = null;
  state.currentContentEl = null;
  state.currentReasoningEl = null;
  state.streamingText = "";
  state.streamingReasoning = "";
  state.deltaBuffer = "";
  state.rafId = null;
  state.streamStartTime = 0;
  state.streamModelLabel = null;
}

// Force-close an orphaned stream that was never terminated by StreamEnd.
// This is a defensive measure — the backend MUST send StreamEnd on all exit
// paths, but if it doesn't, we prevent the UI from getting stuck.
export function forceCloseOrphanedStream(state: StreamState): void {
  if (state.rafId !== null) {
    cancelAnimationFrame(state.rafId);
    state.rafId = null;
  }
  if (state.deltaBuffer) {
    state.streamingText += state.deltaBuffer;
    state.deltaBuffer = "";
  }

  if (state.currentBubble) {
    state.currentBubble.classList.remove("streaming");
    state.currentBubble.querySelector(".streaming-cursor")?.remove();
    // Finalize reasoning section if it was mid-stream
    const reasoningSection = state.currentBubble.querySelector(".reasoning-streaming");
    if (reasoningSection) {
      reasoningSection.classList.remove("reasoning-streaming");
    }
  }

  if (state.currentContentEl) {
    const cleanedText = stripStreamingCursorArtifacts(state.streamingText);
    state.currentContentEl.innerHTML = cleanedText
      ? renderContent(cleanedText)
      : "";
  }

  state.currentBubble = null;
  state.currentContentEl = null;
  state.currentReasoningEl = null;
  state.streamingText = "";
  state.streamingReasoning = "";
  state.streamModelLabel = null;
}

export function stripStreamingCursorArtifacts(text: string): string {
  return text.replace(STREAMING_CURSOR_ARTIFACT_RE, "");
}

export function renderStreamingBody(text: string): string {
  const cleaned = stripStreamingCursorArtifacts(text);
  const body = cleaned ? renderContent(cleaned) : "...";
  return `${body}<span class="streaming-cursor" aria-hidden="true"></span>`;
}

export function formatStreamingReasoningPreview(text: string): string {
  const normalized = normalizeReasoningText(text);
  return normalized.length > 0 ? normalized : "...";
}

function longestSuffixPrefixOverlap(
  existing: string,
  incoming: string,
): number {
  const maxOverlap = Math.min(existing.length, incoming.length);
  for (let len = maxOverlap; len > 0; len -= 1) {
    if (existing.slice(existing.length - len) === incoming.slice(0, len)) {
      return len;
    }
  }
  return 0;
}

function mergeStreamingDelta(existing: string, incoming: string): string {
  if (!incoming) return existing;
  if (!existing) return incoming;
  if (incoming.length >= 8 && existing.includes(incoming)) return existing;
  if (incoming === existing) return existing;
  if (incoming.startsWith(existing)) return incoming;
  if (existing.endsWith(incoming)) return existing;

  const overlap = longestSuffixPrefixOverlap(existing, incoming);
  if (overlap > 0) {
    return existing + incoming.slice(overlap);
  }
  return existing + incoming;
}

export function handleStreamStart(
  state: StreamState,
  appendBubble: (bubble: HTMLElement) => void,
  agent: string,
  modelInfo: unknown,
  resolveModelLabel: (info: unknown) => string | null,
  scrollToBottom: () => void,
): void {
  state.streamStartTime = Date.now();
  state.streamingText = "";
  state.streamingReasoning = "";
  state.deltaBuffer = "";
  state.streamModelLabel = resolveModelLabel(modelInfo);
  if (state.rafId !== null) {
    cancelAnimationFrame(state.rafId);
    state.rafId = null;
  }

  const bubble = document.createElement("div");
  bubble.className = "message assistant-message streaming";
  bubble.dataset.testid = "chat-message";

  if (agent && agent !== "tycode") {
    const label = document.createElement("div");
    label.className = "agent-label";
    label.textContent = agent;
    bubble.appendChild(label);
  }

  const contentEl = document.createElement("div");
  contentEl.className = "message-content";
  contentEl.innerHTML = renderStreamingBody("");
  bubble.appendChild(contentEl);

  appendBubble(bubble);
  state.currentBubble = bubble;
  state.currentContentEl = contentEl;
  state.currentReasoningEl = null;
  scrollToBottom();
}

export function handleStreamDelta(
  state: StreamState,
  text: string,
  scrollToBottom: () => void,
): void {
  if (!state.currentContentEl) return;
  state.deltaBuffer += text;
  if (state.rafId !== null) return;
  state.rafId = requestAnimationFrame(() =>
    flushDeltaBuffer(state, scrollToBottom),
  );
}

export function flushDeltaBuffer(
  state: StreamState,
  scrollToBottom: () => void,
): void {
  state.rafId = null;
  if (!state.currentContentEl || !state.deltaBuffer) return;
  state.streamingText += state.deltaBuffer;
  state.deltaBuffer = "";
  state.currentContentEl.innerHTML = renderStreamingBody(state.streamingText);
  scrollToBottom();
}

export function handleStreamEnd(
  state: StreamState,
  message: ChatMessage,
  createMessageElement: (msg: ChatMessage) => HTMLElement,
  resolveModelLabel: (info: unknown) => string | null,
  scrollToBottom: () => void,
): { durationMs: number } | null {
  if (state.rafId !== null) {
    cancelAnimationFrame(state.rafId);
    state.rafId = null;
  }
  if (state.deltaBuffer) {
    state.streamingText += state.deltaBuffer;
    state.deltaBuffer = "";
  }

  if (!state.currentBubble) return null;

  state.currentBubble.classList.remove("streaming");

  if (
    !resolveModelLabel((message as any).model_info) &&
    state.streamModelLabel
  ) {
    (message as any).model_info = { model: state.streamModelLabel };
  }

  const embeddedToolCalls = state.currentBubble.querySelector(
    ".embedded-tool-calls",
  );

  const hasReasoning =
    !!message.reasoning &&
    typeof message.reasoning.text === "string" &&
    message.reasoning.text.trim().length > 0;
  const fallbackReasoning = normalizeReasoningText(state.streamingReasoning);
  const finalizedMessage: ChatMessage =
    hasReasoning || state.streamingReasoning.trim().length === 0
      ? message
      : {
          ...message,
          reasoning: { text: fallbackReasoning || state.streamingReasoning },
        };

  const rendered = createMessageElement(finalizedMessage);
  if (!rendered.hasChildNodes()) {
    console.warn(
      "[stream] StreamEnd rendered element has no children — bubble content may vanish",
    );
  }

  if (embeddedToolCalls) {
    const metaBar = rendered.querySelector(".message-footer");
    if (metaBar) {
      rendered.insertBefore(embeddedToolCalls, metaBar);
    } else {
      rendered.appendChild(embeddedToolCalls);
    }
  }

  const durationMs = Date.now() - state.streamStartTime;

  state.lastRenderedBubble = rendered;
  state.currentBubble = null;
  state.currentContentEl = null;
  state.currentReasoningEl = null;
  state.streamingText = "";
  state.streamingReasoning = "";
  state.streamModelLabel = null;

  return { durationMs };
}

export function handleStreamInterruption(
  state: StreamState,
  errorMessage: string,
  scrollToBottom: () => void,
): void {
  if (state.rafId !== null) {
    cancelAnimationFrame(state.rafId);
    state.rafId = null;
  }
  if (state.deltaBuffer) {
    state.streamingText += state.deltaBuffer;
    state.deltaBuffer = "";
  }

  state.currentBubble?.classList.remove("streaming");

  if (!state.currentBubble || !state.currentContentEl) return;

  const cleanedStreamingText = stripStreamingCursorArtifacts(
    state.streamingText,
  );
  if (cleanedStreamingText) {
    state.currentContentEl.innerHTML = renderContent(cleanedStreamingText);
  } else {
    state.currentContentEl.textContent = "";
  }

  const banner = document.createElement("div");
  banner.className = "stream-error-banner";
  banner.innerHTML = `<span class="stream-error-icon">⚠</span><span class="stream-error-text">Stream interrupted: ${escapeHtml(errorMessage)}</span>`;

  state.currentBubble.appendChild(banner);

  state.currentBubble = null;
  state.currentContentEl = null;
  state.currentReasoningEl = null;
  state.streamingText = "";
  state.streamingReasoning = "";
  state.streamModelLabel = null;
  scrollToBottom();
}

export function handleReasoningDelta(
  state: StreamState,
  text: string,
  scrollToBottom: () => void,
): void {
  if (!state.currentBubble) return;

  state.streamingReasoning = mergeStreamingDelta(
    state.streamingReasoning,
    text,
  );

  if (!state.currentReasoningEl) {
    const section = document.createElement("div");
    section.className = "reasoning-section reasoning-streaming";

    const header = document.createElement("div");
    header.className = "reasoning-header";
    header.innerHTML = '<span class="reasoning-icon">💭</span> Thinking...';

    const chevron = document.createElement("span");
    chevron.className = "reasoning-chevron";
    chevron.textContent = "▶";
    header.appendChild(chevron);

    const content = document.createElement("div");
    content.className = "reasoning-stream-line";
    content.textContent = "...";

    section.appendChild(header);
    section.appendChild(content);

    state.currentBubble.insertBefore(section, state.currentContentEl);
    state.currentReasoningEl = content;
  }

  state.currentReasoningEl.textContent = formatStreamingReasoningPreview(
    state.streamingReasoning,
  );
  state.currentReasoningEl.scrollTop = state.currentReasoningEl.scrollHeight;
  scrollToBottom();
}
