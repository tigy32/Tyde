import type { ChatMessage, MessageSender, TokenUsage } from "@tyde/protocol";
import {
  hideTruncationIfNotNeeded,
  renderContent,
  wrapWithTruncation,
} from "../renderer";
import { normalizeReasoningText } from "./reasoning";

export function formatCompact(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return String(n);
}

export function senderKey(sender: MessageSender): string {
  if (typeof sender === "string") return sender;
  if ("Assistant" in sender) return `Assistant:${sender.Assistant.agent}`;
  return "Unknown";
}

export function senderClass(sender: MessageSender): string {
  if (sender === "User") return "user-message";
  if (sender === "System") return "system-message";
  if (sender === "Warning") return "warning-message";
  if (sender === "Error") return "error-message";
  if (typeof sender === "object" && "Assistant" in sender)
    return "assistant-message";
  return "system-message";
}

export function extractAgentName(sender: MessageSender): string | null {
  if (typeof sender === "object" && "Assistant" in sender) {
    return sender.Assistant.agent;
  }
  return null;
}

export function resolveModelLabel(modelInfo: unknown): string | null {
  if (!modelInfo || typeof modelInfo !== "object") return null;
  const model = (modelInfo as Record<string, unknown>).model;
  return typeof model === "string" ? model : null;
}

export function formatRelativeTime(epochMs: number): string {
  const now = Date.now();
  const diffMs = now - epochMs;
  const diffSec = Math.floor(diffMs / 1000);
  const diffMin = Math.floor(diffSec / 60);
  const diffHr = Math.floor(diffMin / 60);

  if (diffSec < 60) return "just now";
  if (diffMin < 60) return `${diffMin}m ago`;
  if (diffHr < 24) return `${diffHr}h ago`;

  const date = new Date(epochMs);
  const today = new Date();
  const yesterday = new Date(today);
  yesterday.setDate(yesterday.getDate() - 1);

  if (date.toDateString() === yesterday.toDateString()) return "yesterday";
  return date.toLocaleDateString();
}

export function setRelativeTimeElement(el: HTMLElement, epochMs: number): void {
  el.dataset.epochMs = String(epochMs);
  el.textContent = formatRelativeTime(epochMs);
}

export function refreshRelativeTimes(container: HTMLElement): void {
  const nodes = container.querySelectorAll<HTMLElement>(
    ".message-timestamp, .message-meta-time",
  );
  for (const node of nodes) {
    const epochRaw = node.dataset.epochMs;
    if (!epochRaw) continue;
    const epoch = Number(epochRaw);
    if (!Number.isFinite(epoch)) continue;
    node.textContent = formatRelativeTime(epoch);
  }
}

export async function copyToClipboard(
  btn: HTMLButtonElement,
  text: string,
): Promise<void> {
  const defaultLabel = btn.dataset.copyDefault || btn.textContent || "Copy";
  try {
    await navigator.clipboard.writeText(text);
    btn.textContent = btn.dataset.copySuccess || "✓";
    btn.classList.remove("copy-failed");
    btn.classList.add("copied");
    setTimeout(() => {
      btn.classList.remove("copied");
      btn.textContent = defaultLabel;
    }, 1200);
  } catch (_err) {
    btn.textContent = btn.dataset.copyError || "!";
    btn.classList.remove("copied");
    btn.classList.add("copy-failed");
    setTimeout(() => {
      btn.classList.remove("copy-failed");
      btn.textContent = defaultLabel;
    }, 1200);
  }
}

export function createTokenBadge(
  usage: TokenUsage,
  modelName: string | null,
  agentName: string | null = null,
): HTMLElement {
  const badge = document.createElement("span");
  badge.className = "token-badge";
  const toToken = (value: unknown): number => {
    const n = Number(value);
    return Number.isFinite(n) && n > 0 ? n : 0;
  };
  const inputTokens = toToken((usage as any).input_tokens);
  const outputTokens = toToken((usage as any).output_tokens);
  const cachedHits = toToken((usage as any).cached_prompt_tokens);
  const cacheWrites = toToken((usage as any).cache_creation_input_tokens);
  const reasoningTokens = toToken((usage as any).reasoning_tokens);

  // Display total prompt-side tokens, including cache hits and cache writes.
  const displayInputTokens = inputTokens + cachedHits + cacheWrites;
  // output_tokens already includes reasoning per contract — no addition needed.
  const displayOutputTokens = outputTokens;

  let inputText = `↑${formatCompact(displayInputTokens)}`;
  if (cachedHits > 0) inputText += ` (cached ${formatCompact(cachedHits)})`;

  let outputText = `↓${formatCompact(displayOutputTokens)}`;
  if (reasoningTokens > 0)
    outputText += ` (reasoning ${formatCompact(reasoningTokens)})`;

  const parts: Array<{ text: string; cls: string }> = [
    ...(modelName
      ? [{ text: modelName, cls: "token-stat token-stat-model" }]
      : []),
    ...(agentName
      ? [{ text: agentName, cls: "token-stat token-stat-agent" }]
      : []),
    { text: inputText, cls: "token-stat token-stat-input" },
    { text: outputText, cls: "token-stat token-stat-output" },
  ];

  parts.forEach((part, idx) => {
    const span = document.createElement("span");
    span.className = part.cls;
    span.textContent = part.text;
    badge.appendChild(span);
    if (idx < parts.length - 1) {
      const sep = document.createElement("span");
      sep.className = "token-sep";
      sep.textContent = "·";
      badge.appendChild(sep);
    }
  });

  badge.title = `Input ${displayInputTokens} (base ${inputTokens} + cache hits ${cachedHits} + cache writes ${cacheWrites}), Output ${displayOutputTokens} (incl. reasoning ${reasoningTokens})`;

  return badge;
}

export function createReasoningSection(
  text: string,
  tokens?: number,
): HTMLElement {
  const section = document.createElement("div");
  section.className = "reasoning-section";

  const header = document.createElement("div");
  header.className = "reasoning-header";
  const tokenLabel =
    Number.isFinite(tokens) && (tokens as number) >= 0
      ? ` <span class="reasoning-tokens">(${tokens} tokens)</span>`
      : "";
  header.innerHTML = `<span class="reasoning-icon">💭</span> Thinking${tokenLabel}`;

  const chevron = document.createElement("span");
  chevron.className = "reasoning-chevron";
  chevron.textContent = "▶";
  header.appendChild(chevron);

  const content = document.createElement("div");
  content.className = "reasoning-content";
  content.textContent = text;

  header.addEventListener("click", () => {
    const isExpanded = content.classList.contains("expanded");
    if (isExpanded) {
      content.classList.remove("expanded");
      chevron.textContent = "▶";
    } else {
      content.classList.add("expanded");
      chevron.textContent = "▼";
    }
  });

  section.appendChild(header);
  section.appendChild(content);
  return section;
}

export function createAssistantMetaBar(
  message: ChatMessage,
  resolveModelLabelFn: (info: unknown) => string | null,
  copyToClipboardFn: (btn: HTMLButtonElement, text: string) => void,
  setRelativeTimeElementFn: (el: HTMLElement, epochMs: number) => void,
): HTMLElement {
  const footer = document.createElement("div");
  footer.className = "message-footer assistant-meta-bar";

  const left = document.createElement("div");
  left.className = "message-meta-left";
  const right = document.createElement("div");
  right.className = "message-meta-right";

  const modelLabel = resolveModelLabelFn((message as any).model_info);
  const agentName = extractAgentName(message.sender);

  if (message.token_usage) {
    const badge = createTokenBadge(message.token_usage, modelLabel, agentName);
    left.appendChild(badge);
  } else if (modelLabel || agentName) {
    const badgeOnly = document.createElement("span");
    badgeOnly.className = "token-badge";
    if (modelLabel) {
      const modelPart = document.createElement("span");
      modelPart.className = "token-stat token-stat-model";
      modelPart.textContent = modelLabel;
      badgeOnly.appendChild(modelPart);
    }
    if (agentName) {
      if (modelLabel) {
        const sep = document.createElement("span");
        sep.className = "token-sep";
        sep.textContent = "·";
        badgeOnly.appendChild(sep);
      }
      const agentPart = document.createElement("span");
      agentPart.className = "token-stat token-stat-agent";
      agentPart.textContent = agentName;
      badgeOnly.appendChild(agentPart);
    }
    left.appendChild(badgeOnly);
  }

  if (message.timestamp) {
    const ts = document.createElement("span");
    ts.className = "message-meta-item message-meta-time";
    setRelativeTimeElementFn(ts, message.timestamp);
    right.appendChild(ts);
  }

  const copyBtn = document.createElement("button");
  copyBtn.className = "message-copy-icon-btn";
  copyBtn.type = "button";
  copyBtn.dataset.copyDefault = "⧉";
  copyBtn.dataset.copySuccess = "✓";
  copyBtn.dataset.copyError = "!";
  copyBtn.textContent = copyBtn.dataset.copyDefault;
  copyBtn.setAttribute("aria-label", "Copy message");
  copyBtn.title = "Copy message";
  copyBtn.addEventListener("click", () => {
    copyToClipboardFn(copyBtn, message.content);
  });

  right.appendChild(copyBtn);
  footer.appendChild(left);
  footer.appendChild(right);
  return footer;
}

export function createMessageElement(
  message: ChatMessage,
  resolveModelLabelFn: (info: unknown) => string | null,
  copyToClipboardFn: (btn: HTMLButtonElement, text: string) => void,
  openLightbox: (src: string) => void,
  setRelativeTimeElementFn: (el: HTMLElement, epochMs: number) => void,
): HTMLElement {
  const el = document.createElement("div");
  const sc = senderClass(message.sender);
  el.className = `message ${sc}`;
  el.dataset.testid = "chat-message";
  el.setAttribute("role", "article");

  if (sc === "assistant-message") {
    const displayedReasoning = resolveDisplayedReasoning(message);
    if (displayedReasoning) {
      el.appendChild(
        createReasoningSection(
          displayedReasoning.text,
          displayedReasoning.tokens,
        ),
      );
    }
  }

  const contentEl = document.createElement("div");
  contentEl.className = "message-content";

  if (sc === "user-message") {
    contentEl.textContent = message.content;
  } else {
    contentEl.innerHTML = wrapWithTruncation(
      renderContent(message.content),
      message.content.length,
    );
    hideTruncationIfNotNeeded(contentEl);
  }
  el.appendChild(contentEl);

  if (message.images && message.images.length > 0) {
    const imagesRow = document.createElement("div");
    imagesRow.className = "message-images";
    for (const img of message.images) {
      const imgEl = document.createElement("img");
      imgEl.src = `data:${img.media_type};base64,${img.data}`;
      imgEl.alt = "attached image";
      imgEl.className = "message-image-thumb";
      imgEl.addEventListener("click", () => openLightbox(imgEl.src));
      imagesRow.appendChild(imgEl);
    }
    el.appendChild(imagesRow);
  }

  if (sc === "assistant-message") {
    el.appendChild(
      createAssistantMetaBar(
        message,
        resolveModelLabelFn,
        copyToClipboardFn,
        setRelativeTimeElementFn,
      ),
    );
  } else if (message.timestamp) {
    const ts = document.createElement("span");
    ts.className = "message-timestamp";
    setRelativeTimeElementFn(ts, message.timestamp);
    el.appendChild(ts);
  }

  return el;
}

function resolveDisplayedReasoning(
  message: ChatMessage,
): { text: string; tokens?: number } | null {
  const rawReasoningText = message.reasoning?.text;
  if (
    typeof rawReasoningText !== "string" ||
    rawReasoningText.trim().length === 0
  ) {
    return null;
  }
  const reasoningText = normalizeReasoningText(rawReasoningText);
  if (reasoningText.length === 0) return null;

  const reasoningTokens = Number(message.reasoning?.tokens);
  const displayTokens =
    Number.isFinite(reasoningTokens) && reasoningTokens >= 0
      ? reasoningTokens
      : undefined;

  return { text: reasoningText, tokens: displayTokens };
}

export function addSystemMessage(
  chatContainer: HTMLElement,
  text: string,
  style: "system" | "warning" | "error",
  scrollToBottom: () => void,
): void {
  const el = document.createElement("div");
  el.className = `message ${style}-message`;
  el.dataset.testid = style === "error" ? "error-message" : "system-message";
  el.setAttribute("role", "article");
  el.textContent = text;
  chatContainer.appendChild(el);
  scrollToBottom();
}
