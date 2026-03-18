import type {
  ToolExecutionResult,
  ToolRequestType,
  ToolUseData,
} from "@tyde/protocol";
import { createTwoFilesPatch } from "diff";
import {
  escapeHtml,
  hideTruncationIfNotNeeded,
  renderContent,
  wrapWithTruncation,
} from "../renderer";

export type ToolOutputMode = "summary" | "compact" | "verbose";
// Legacy alias for existing imports in older code paths.
export type DiffDisplayMode = ToolOutputMode;
const TOOL_OUTPUT_MODE_KEY = "tyde-tool-output-mode";
const LEGACY_DIFF_MODE_KEY = "tyde-diff-display-mode";
const COMPACT_MAX_READ_FILES = 8;
const COMPACT_MAX_SEARCH_TYPES = 12;

function mapLegacyDiffMode(mode: string | null): ToolOutputMode | null {
  if (mode === "none") return "summary";
  if (mode === "capped") return "compact";
  if (mode === "full") return "verbose";
  return null;
}

function loadToolOutputMode(): ToolOutputMode {
  const stored = localStorage.getItem(TOOL_OUTPUT_MODE_KEY);
  if (stored === "summary" || stored === "compact" || stored === "verbose")
    return stored;
  const legacy = mapLegacyDiffMode(localStorage.getItem(LEGACY_DIFF_MODE_KEY));
  if (legacy) return legacy;
  return "compact";
}

function saveToolOutputMode(mode: ToolOutputMode): void {
  localStorage.setItem(TOOL_OUTPUT_MODE_KEY, mode);
}

let currentToolOutputMode: ToolOutputMode = loadToolOutputMode();

const toolOutputUpdateCallbacks = new Set<(mode: ToolOutputMode) => void>();
const modeChangeListeners = new Set<(mode: ToolOutputMode) => void>();

export function getToolOutputMode(): ToolOutputMode {
  return currentToolOutputMode;
}

export function setToolOutputMode(mode: ToolOutputMode): void {
  currentToolOutputMode = mode;
  saveToolOutputMode(mode);
}

export function broadcastToolOutputMode(): void {
  for (const cb of toolOutputUpdateCallbacks) {
    cb(currentToolOutputMode);
  }
  for (const cb of modeChangeListeners) {
    cb(currentToolOutputMode);
  }
  syncAllToolCardExpansion(currentToolOutputMode);
}

export function onToolOutputModeChange(
  cb: (mode: ToolOutputMode) => void,
): void {
  modeChangeListeners.add(cb);
}

export function createToolOutputToggleButton(): HTMLElement {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "tool-output-toggle-global";
  btn.dataset.testid = "tool-output-toggle-global";
  btn.textContent = toolOutputModeIcon(currentToolOutputMode);
  btn.title = toolOutputModeTooltip(currentToolOutputMode);
  btn.addEventListener("click", () => {
    const next = nextToolOutputMode(currentToolOutputMode);
    setToolOutputMode(next);
    broadcastToolOutputMode();
  });
  onToolOutputModeChange((mode) => {
    btn.textContent = toolOutputModeIcon(mode);
    btn.title = toolOutputModeTooltip(mode);
  });
  return btn;
}

// Backward-compatible exports for previous diff-only setting names.
export function getDiffDisplayMode(): DiffDisplayMode {
  return getToolOutputMode();
}
export function setDiffDisplayMode(mode: DiffDisplayMode): void {
  setToolOutputMode(mode);
}
export function broadcastDiffMode(): void {
  broadcastToolOutputMode();
}
export function onDiffModeChange(cb: (mode: DiffDisplayMode) => void): void {
  onToolOutputModeChange(cb);
}
export function createDiffToggleButton(): HTMLElement {
  return createToolOutputToggleButton();
}

export interface ToolState {
  toolCards: Map<string, HTMLElement>;
  diffData: Map<string, { filePath: string; before: string; after: string }>;
  toolDiffByCall: Map<string, string>;
  toolHostByCall: Map<string, HTMLElement>;
}

export function createToolState(): ToolState {
  return {
    toolCards: new Map(),
    diffData: new Map(),
    toolDiffByCall: new Map(),
    toolHostByCall: new Map(),
  };
}

export function resetToolState(state: ToolState): void {
  state.toolCards.clear();
  state.diffData.clear();
  state.toolDiffByCall.clear();
  state.toolHostByCall.clear();
}

// Clears only toolCards — preserves diffData and toolDiffByCall so
// "View Diff" / "Open Diff" buttons in restored HTML still resolve.
export function softResetToolState(state: ToolState): void {
  state.toolCards.clear();
}

export function toolIcon(kind: string): string {
  const icons: Record<string, string> = {
    ModifyFile: "✏",
    RunCommand: "▶",
    ReadFiles: "📄",
    SearchTypes: "🔍",
    GetTypeDocs: "📖",
  };
  return icons[kind] ?? "⚙";
}

const SPAWN_TOOL_NAMES = new Set([
  "Task",
  "Agent",
  "tyde_spawn_agent",
  "tyde_run_agent",
  "spawn_agent",
  "spawnAgent",
  "spawn_subagent",
  "delegate",
]);

export function isSpawnTool(toolName: string): boolean {
  return SPAWN_TOOL_NAMES.has(toolName);
}

function extractSpawnDetail(
  _toolName: string,
  toolType: ToolRequestType,
): string {
  if (toolType.kind !== "Other") return "";
  const args = toolType.args as Record<string, unknown> | null;
  if (!args || typeof args !== "object") return "";

  // Claude Code: Task/Agent tools may include both a short "description" label
  // and the actual instruction in "prompt".
  // MCP: tyde_spawn_agent/tyde_run_agent use "name" and "prompt".
  const name = typeof args.name === "string" ? args.name : null;
  const description =
    typeof args.description === "string" ? args.description : null;
  const subagentType =
    typeof args.subagent_type === "string" ? args.subagent_type : null;
  const prompt = typeof args.prompt === "string" ? args.prompt : null;
  const task = typeof args.task === "string" ? args.task : null;
  const instruction =
    typeof args.instruction === "string" ? args.instruction : null;
  const message = typeof args.message === "string" ? args.message : null;

  const label = name ?? subagentType ?? "";
  const detail = prompt ?? task ?? instruction ?? message ?? description ?? "";

  if (label && detail) return `${label}: ${detail}`;
  return label || detail;
}

export function countLines(text: string): number {
  if (!text) return 0;
  return text.split("\n").length;
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes}B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)}KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)}MB`;
}

export function createPreBlock(text: string): HTMLElement {
  const pre = document.createElement("pre");
  pre.className = "tool-result-pre";
  pre.textContent = text;
  return pre;
}

export function buildCommandOutputBlock(
  output: string,
  className: string,
  fullHeight = false,
): HTMLElement {
  const wrapper = document.createElement("div");
  wrapper.className = `tool-result-output ${className}`;
  if (fullHeight) {
    wrapper.appendChild(createPreBlock(output));
  } else {
    const pre = createPreBlock(output);
    wrapper.innerHTML = wrapWithTruncation(pre.outerHTML, output.length, 0);
    hideTruncationIfNotNeeded(wrapper);
  }
  return wrapper;
}

export function createToolResultSection(
  title: string,
  content: HTMLElement,
  open: boolean,
): HTMLElement {
  const details = document.createElement("details");
  details.className = "tool-result-section";
  details.open = open;

  const summary = document.createElement("summary");
  summary.className = "tool-result-section-title";
  summary.textContent = title;

  details.append(summary, content);
  return details;
}

function renderInlineDiff(
  before: string,
  after: string,
  filePath: string,
  truncate: boolean,
): HTMLElement {
  const patch = createTwoFilesPatch(filePath, filePath, before, after, "", "", {
    context: 2,
  });
  const lines = patch.split("\n");

  let startIdx = 0;
  for (let i = 0; i < lines.length; i++) {
    if (lines[i].startsWith("@@")) {
      startIdx = i;
      break;
    }
  }

  const diffLines = lines.slice(startIdx);

  const container = document.createElement("div");
  container.className = "inline-diff-preview";

  const pre = document.createElement("pre");
  pre.className = "inline-diff-code";

  for (const line of diffLines) {
    const lineEl = document.createElement("div");
    lineEl.className = "inline-diff-line";

    if (line.startsWith("+")) {
      lineEl.classList.add("inline-diff-added");
    } else if (line.startsWith("-")) {
      lineEl.classList.add("inline-diff-removed");
    } else if (line.startsWith("@@")) {
      lineEl.classList.add("inline-diff-hunk");
    } else {
      lineEl.classList.add("inline-diff-context");
    }

    lineEl.textContent = line;
    pre.appendChild(lineEl);
  }

  container.appendChild(pre);

  if (truncate) {
    const wrapper = document.createElement("div");
    wrapper.innerHTML = wrapWithTruncation(
      container.outerHTML,
      diffLines.length,
      0,
    );
    hideTruncationIfNotNeeded(wrapper);
    return wrapper;
  }

  return container;
}

function nextToolOutputMode(current: ToolOutputMode): ToolOutputMode {
  if (current === "summary") return "compact";
  if (current === "compact") return "verbose";
  return "summary";
}

function toolOutputModeIcon(mode: ToolOutputMode): string {
  if (mode === "summary") return "⊘";
  if (mode === "compact") return "◐";
  return "◉";
}

function toolOutputModeTooltip(mode: ToolOutputMode): string {
  if (mode === "summary") return "Tool outputs: summary";
  if (mode === "compact") return "Tool outputs: compact previews";
  return "Tool outputs: verbose";
}

function createMetaLine(text: string): HTMLElement {
  const line = document.createElement("div");
  line.className = "tool-request-meta";
  line.textContent = text;
  return line;
}

function summarizeSingleLine(text: string, maxChars = 160): string {
  const normalized = text.replace(/\s+/g, " ").trim();
  if (normalized.length <= maxChars) return normalized;
  return `${normalized.slice(0, Math.max(0, maxChars - 1))}…`;
}

function suffixForCount(
  count: number,
  singular: string,
  plural: string,
): string {
  return count === 1 ? singular : plural;
}

function formatHeaderDetail(text: string, maxChars = 88): string {
  return summarizeSingleLine(text, maxChars);
}

function safeJsonStringify(value: unknown, pretty: boolean): string {
  try {
    const serialized = pretty
      ? JSON.stringify(value, null, 2)
      : JSON.stringify(value);
    return serialized ?? String(value);
  } catch (err) {
    console.error("Failed to stringify JSON value:", err);
    return String(value);
  }
}

function countSummaryLines(text: string): number {
  if (!text.trim()) return 0;
  return countLines(text);
}

function bindToolOutputRenderer(
  root: HTMLElement,
  render: (mode: ToolOutputMode) => void,
): void {
  let firstRender = true;
  const update = (mode: ToolOutputMode) => {
    if (!firstRender && !root.isConnected) {
      toolOutputUpdateCallbacks.delete(update);
      return;
    }
    firstRender = false;
    render(mode);
  };
  update(currentToolOutputMode);
  toolOutputUpdateCallbacks.add(update);
}

function shouldExpandToolDetailsOnRequest(mode: ToolOutputMode): boolean {
  return mode !== "summary";
}

function shouldExpandToolDetailsOnCompletion(
  mode: ToolOutputMode,
  success: boolean,
): boolean {
  if (!success) return true;
  return mode !== "summary";
}

function setCardExpandedState(card: HTMLElement, expanded: boolean): void {
  const details = card.querySelector(".tool-details") as HTMLElement | null;
  const chevron = card.querySelector(".tool-chevron");
  if (!details) return;
  if (expanded) {
    details.classList.add("expanded");
    if (chevron) chevron.textContent = "▼";
    return;
  }
  details.classList.remove("expanded");
  if (chevron) chevron.textContent = "▶";
}

function completionStateFromCard(card: HTMLElement): boolean | null {
  const statusEl = card.querySelector(
    ".tool-status-text",
  ) as HTMLElement | null;
  if (!statusEl) return null;
  if (statusEl.classList.contains("failure")) return false;
  if (statusEl.classList.contains("success")) return true;
  return null;
}

function syncCardExpansionForMode(
  card: HTMLElement,
  mode: ToolOutputMode,
): void {
  const details = card.querySelector(".tool-details") as HTMLElement | null;
  if (!details || details.childElementCount === 0) return;
  const completionState = completionStateFromCard(card);
  if (completionState === null) {
    setCardExpandedState(card, shouldExpandToolDetailsOnRequest(mode));
    return;
  }
  setCardExpandedState(
    card,
    shouldExpandToolDetailsOnCompletion(mode, completionState),
  );
}

function syncAllToolCardExpansion(mode: ToolOutputMode): void {
  const cards = document.querySelectorAll<HTMLElement>(".tool-card");
  for (const card of cards) {
    syncCardExpansionForMode(card, mode);
  }
}

function ensureCardHeaderDetailElement(card: HTMLElement): HTMLElement | null {
  const header = card.querySelector(".tool-card-header") as HTMLElement | null;
  if (!header) return null;
  let detail = header.querySelector(
    ".tool-header-detail",
  ) as HTMLElement | null;
  if (detail) return detail;
  detail = document.createElement("span");
  detail.className = "tool-header-detail";
  const statusEl = header.querySelector(".tool-status-text");
  if (statusEl) {
    header.insertBefore(detail, statusEl);
  } else {
    header.appendChild(detail);
  }
  return detail;
}

function setCardHeaderDetail(
  card: HTMLElement,
  detailText: string,
  storeAsBase = false,
): void {
  const detail = ensureCardHeaderDetailElement(card);
  if (!detail) return;
  detail.textContent = detailText;
  if (storeAsBase) {
    card.dataset.headerDetailBase = detailText;
  }
}

function cardHeaderBaseDetail(card: HTMLElement): string | null {
  const value = card.dataset.headerDetailBase;
  return value && value.length > 0 ? value : null;
}

function toolRequestHeaderDetail(toolType: ToolRequestType): string {
  switch (toolType.kind) {
    case "ModifyFile":
      return formatHeaderDetail(toolType.file_path, 72);
    case "RunCommand":
      return formatHeaderDetail(toolType.command, 96);
    case "ReadFiles":
      if (toolType.file_paths.length === 1) {
        return formatHeaderDetail(toolType.file_paths[0], 72);
      }
      return `${toolType.file_paths.length} ${suffixForCount(toolType.file_paths.length, "file", "files")}`;
    case "SearchTypes":
      return `search ${toolType.type_name}`;
    case "GetTypeDocs":
      return `docs ${toolType.type_path}`;
    case "Other":
      return "tool call";
  }
}

function completionHeaderDetail(
  card: HTMLElement,
  result: ToolExecutionResult,
  toolName: string,
): string {
  const base = cardHeaderBaseDetail(card);
  switch (result.kind) {
    case "ModifyFile":
      return `${base ?? "file"} · +${result.lines_added} -${result.lines_removed}`;
    case "RunCommand": {
      const parts = [base ?? "run command", `exit ${result.exit_code}`];
      const stdoutLines = countSummaryLines(result.stdout);
      const stderrLines = countSummaryLines(result.stderr);
      if (stdoutLines > 0) parts.push(`out ${stdoutLines}L`);
      if (stderrLines > 0) parts.push(`err ${stderrLines}L`);
      return parts.join(" · ");
    }
    case "ReadFiles": {
      const totalBytes = result.files.reduce(
        (sum, file) => sum + file.bytes,
        0,
      );
      const fileCount = result.files.length;
      if (fileCount === 1) {
        return `${formatHeaderDetail(result.files[0].path, 72)} · ${formatBytes(totalBytes)}`;
      }
      return `read ${fileCount} ${suffixForCount(fileCount, "file", "files")} · ${formatBytes(totalBytes)}`;
    }
    case "SearchTypes":
      return `${result.types.length} matching ${suffixForCount(result.types.length, "type", "types")}`;
    case "GetTypeDocs": {
      const docs = result.documentation || "";
      return docs.trim().length === 0
        ? "no documentation"
        : `documentation · ${countSummaryLines(docs)}L`;
    }
    case "Error":
      return `error · ${summarizeSingleLine(result.short_message, 90)}`;
    case "Other": {
      if (isSpawnTool(toolName) && typeof result.result === "string") {
        return `response · ${formatBytes(result.result.length)}`;
      }
      const serialized = safeJsonStringify(result.result, false);
      return `result json · ${formatBytes(serialized.length)}`;
    }
  }
}

export function toolRequestSummary(
  state: ToolState,
  toolCallId: string,
  toolName: string,
  toolType: ToolRequestType,
): HTMLElement | null {
  switch (toolType.kind) {
    case "ModifyFile": {
      const diffId = `view-diff-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`;
      state.toolDiffByCall.set(toolCallId, diffId);
      state.diffData.set(diffId, {
        filePath: toolType.file_path,
        before: toolType.before,
        after: toolType.after,
      });
      return null;
    }
    case "RunCommand":
      return null;
    case "ReadFiles": {
      if (toolType.file_paths.length === 1) return null;
      const wrap = document.createElement("div");
      wrap.className = "tool-request-summary";
      const preview = toolType.file_paths
        .slice(0, 3)
        .map((p) => `<code class="inline-code">${escapeHtml(p)}</code>`)
        .join(", ");
      const extra =
        toolType.file_paths.length > 3
          ? ` +${toolType.file_paths.length - 3} more`
          : "";
      wrap.innerHTML = `<div class="tool-request-row">${preview}${extra}</div>`;
      return wrap;
    }
    case "SearchTypes": {
      const wrap = document.createElement("div");
      wrap.className = "tool-request-summary";
      wrap.innerHTML = `<div class="tool-request-row">Search: <code class="inline-code">${escapeHtml(toolType.type_name)}</code></div>
          <div class="tool-request-meta">${escapeHtml(toolType.workspace_root)}</div>`;
      return wrap;
    }
    case "GetTypeDocs": {
      const wrap = document.createElement("div");
      wrap.className = "tool-request-summary";
      wrap.innerHTML = `<div class="tool-request-row">Docs: <code class="inline-code">${escapeHtml(toolType.type_path)}</code></div>
          <div class="tool-request-meta">${escapeHtml(toolType.workspace_root)}</div>`;
      return wrap;
    }
    case "Other": {
      if (isSpawnTool(toolName)) {
        const spawnDetail = extractSpawnDetail(toolName, toolType);
        if (spawnDetail) {
          const wrap = document.createElement("div");
          wrap.className = "tool-request-summary";
          wrap.innerHTML = `<div class="tool-request-row">${escapeHtml(spawnDetail)}</div>`;
          return wrap;
        }
      }
      return null;
    }
  }
}

export function toolResultElement(
  state: ToolState,
  toolCallId: string,
  result: ToolExecutionResult,
  toolName: string,
): HTMLElement | null {
  switch (result.kind) {
    case "ModifyFile": {
      const root = document.createElement("div");
      root.className = "tool-result tool-result-modify";
      const diffId = state.toolDiffByCall.get(toolCallId);
      const diffEntry = diffId ? state.diffData.get(diffId) : undefined;
      const updateResult = (mode: ToolOutputMode) => {
        root.replaceChildren();
        if (mode === "summary" || !diffEntry) return;
        const diffContainer = document.createElement("div");
        diffContainer.className = "inline-diff-container";
        diffContainer.appendChild(
          renderInlineDiff(
            diffEntry.before,
            diffEntry.after,
            diffEntry.filePath,
            mode === "compact",
          ),
        );
        root.appendChild(diffContainer);
      };
      bindToolOutputRenderer(root, updateResult);
      return root;
    }
    case "RunCommand": {
      const root = document.createElement("div");
      root.className = "tool-result tool-result-command";
      const updateResult = (mode: ToolOutputMode) => {
        root.replaceChildren();
        const stdoutLines = countSummaryLines(result.stdout);
        const stderrLines = countSummaryLines(result.stderr);
        const hasStdout = stdoutLines > 0;
        const hasStderr = stderrLines > 0;

        if (mode === "summary") {
          return;
        }

        const fullHeight = mode === "verbose";

        if (!hasStdout && !hasStderr) {
          return;
        }

        if (hasStdout) {
          root.appendChild(
            buildCommandOutputBlock(
              result.stdout,
              "tool-result-stdout",
              fullHeight,
            ),
          );
        }

        if (hasStderr) {
          root.appendChild(
            buildCommandOutputBlock(
              result.stderr,
              "tool-result-stderr",
              fullHeight,
            ),
          );
        }
      };
      bindToolOutputRenderer(root, updateResult);
      return root;
    }
    case "ReadFiles": {
      const root = document.createElement("div");
      root.className = "tool-result tool-result-read";
      const updateResult = (mode: ToolOutputMode) => {
        root.replaceChildren();
        if (
          mode === "summary" ||
          (mode === "compact" && result.files.length === 1)
        ) {
          return;
        }

        const visible =
          mode === "compact"
            ? result.files.slice(0, COMPACT_MAX_READ_FILES)
            : result.files;
        for (const file of visible) {
          const row = document.createElement("div");
          row.className = "tool-result-file";
          row.innerHTML = `<span class="tool-result-icon">📄</span><code class="inline-code">${escapeHtml(file.path)}</code> <span class="tool-result-bytes">${formatBytes(file.bytes)}</span>`;
          root.appendChild(row);
        }

        const hiddenCount = result.files.length - visible.length;
        if (hiddenCount > 0) {
          const suffix = hiddenCount === 1 ? "" : "s";
          root.appendChild(
            createMetaLine(`+${hiddenCount} more file${suffix}`),
          );
        }
      };
      bindToolOutputRenderer(root, updateResult);
      return root;
    }
    case "SearchTypes": {
      const root = document.createElement("div");
      root.className = "tool-result tool-result-search";
      const updateResult = (mode: ToolOutputMode) => {
        root.replaceChildren();
        const total = result.types.length;
        if (mode === "summary") {
          root.appendChild(
            createMetaLine(
              total === 0
                ? "No matching types found"
                : `Found ${total} matching types`,
            ),
          );
          return;
        }
        const visible =
          mode === "compact"
            ? result.types.slice(0, COMPACT_MAX_SEARCH_TYPES)
            : result.types;
        if (visible.length === 0) {
          root.appendChild(createMetaLine("No matching types found"));
          return;
        }
        for (const typeName of visible) {
          const code = document.createElement("code");
          code.className = "inline-code";
          code.textContent = typeName;
          root.appendChild(code);
        }
        const hiddenCount = total - visible.length;
        if (hiddenCount > 0) {
          root.appendChild(createMetaLine(`+${hiddenCount} more`));
        }
      };
      bindToolOutputRenderer(root, updateResult);
      return root;
    }
    case "GetTypeDocs": {
      const root = document.createElement("div");
      root.className = "tool-result tool-result-docs";
      const documentation = result.documentation || "";
      const updateResult = (mode: ToolOutputMode) => {
        root.replaceChildren();
        if (mode === "summary") {
          if (!documentation.trim()) {
            root.appendChild(createMetaLine("No documentation returned"));
            return;
          }
          root.appendChild(
            createMetaLine(
              `Documentation · ${countSummaryLines(documentation)} lines`,
            ),
          );
          return;
        }
        const fullHeight = mode === "verbose";
        root.appendChild(
          createToolResultSection(
            "Documentation",
            buildCommandOutputBlock(
              documentation || "No documentation returned",
              "tool-result-stdout",
              fullHeight,
            ),
            fullHeight,
          ),
        );
      };
      bindToolOutputRenderer(root, updateResult);
      return root;
    }
    case "Error": {
      const root = document.createElement("div");
      root.className = "tool-result tool-result-command";
      const updateResult = (mode: ToolOutputMode) => {
        root.replaceChildren();
        if (mode === "summary") {
          root.appendChild(
            createMetaLine(
              `Error: ${summarizeSingleLine(result.short_message)}`,
            ),
          );
          return;
        }
        const fullHeight = mode === "verbose";
        const header = document.createElement("div");
        header.className = "tool-result-command-header";
        const headerLabel = document.createElement("span");
        headerLabel.className = "tool-result-exit-failure";
        headerLabel.textContent = "Error";
        header.appendChild(headerLabel);
        root.appendChild(header);

        root.appendChild(
          buildCommandOutputBlock(
            result.short_message,
            "tool-result-stderr",
            fullHeight,
          ),
        );

        if (result.detailed_message) {
          root.appendChild(
            createToolResultSection(
              "Details",
              buildCommandOutputBlock(
                result.detailed_message,
                "tool-result-stderr",
                fullHeight,
              ),
              true,
            ),
          );
        }
      };
      bindToolOutputRenderer(root, updateResult);
      return root;
    }
    case "Other": {
      if (isSpawnTool(toolName) && typeof result.result === "string") {
        return renderSpawnToolResult(result.result);
      }
      const root = document.createElement("div");
      root.className = "tool-result tool-result-docs";
      const jsonCompact = safeJsonStringify(result.result, false);
      const jsonPretty = safeJsonStringify(result.result, true);
      const updateResult = (mode: ToolOutputMode) => {
        root.replaceChildren();
        if (mode === "summary") {
          root.appendChild(
            createMetaLine(`Result JSON · ${formatBytes(jsonCompact.length)}`),
          );
          return;
        }
        const fullHeight = mode === "verbose";
        root.appendChild(
          createToolResultSection(
            "Result JSON",
            buildCommandOutputBlock(
              jsonPretty,
              "tool-result-stdout",
              fullHeight,
            ),
            fullHeight,
          ),
        );
      };
      bindToolOutputRenderer(root, updateResult);
      return root;
    }
  }
}

function renderSpawnToolResult(text: string): HTMLElement {
  const root = document.createElement("div");
  root.className = "tool-result tool-result-spawn";
  const rendered = renderContent(text);
  const updateResult = (mode: ToolOutputMode) => {
    root.replaceChildren();
    if (mode === "summary") return;
    const content = document.createElement("div");
    content.className = "message-content spawn-result-content";
    content.innerHTML =
      mode === "verbose"
        ? rendered
        : wrapWithTruncation(rendered, text.length, 0);
    root.appendChild(content);
    hideTruncationIfNotNeeded(content);
  };
  bindToolOutputRenderer(root, updateResult);
  return root;
}

function createToggleHandler(
  details: HTMLElement,
  chevron: HTMLElement,
): () => void {
  return () => {
    const isExpanded = details.classList.contains("expanded");
    if (isExpanded) {
      details.classList.remove("expanded");
      chevron.textContent = "▶";
      return;
    }
    details.classList.add("expanded");
    chevron.textContent = "▼";
  };
}

function updatePendingCard(
  card: HTMLElement,
  state: ToolState,
  toolCallId: string,
  toolName: string,
  toolType: ToolRequestType,
): void {
  const statusEl = card.querySelector(".tool-status-text");
  if (statusEl) {
    statusEl.textContent = isSpawnTool(toolName) ? "Spawned" : "Running...";
    statusEl.classList.remove("pending");
  }
  const iconEl = card.querySelector(".tool-status-icon");
  if (iconEl) {
    iconEl.textContent = isSpawnTool(toolName) ? "🤖" : toolIcon(toolType.kind);
  }
  setCardHeaderDetail(card, toolRequestHeaderDetail(toolType), true);
  const details = card.querySelector(".tool-details") as HTMLElement | null;
  if (!details) return;
  for (const existingSummary of Array.from(
    details.querySelectorAll(":scope > .tool-request-summary"),
  )) {
    existingSummary.remove();
  }
  const summary = toolRequestSummary(state, toolCallId, toolName, toolType);
  if (!summary) return;
  details.appendChild(summary);
  setCardExpandedState(
    card,
    shouldExpandToolDetailsOnRequest(currentToolOutputMode),
  );
}

export function createPendingToolCards(
  state: ToolState,
  toolCalls: ToolUseData[],
  appendTarget: HTMLElement,
  scrollToBottom: () => void,
): void {
  if (toolCalls.length === 0) return;

  let toolCallsContainer = appendTarget.querySelector(
    ":scope > .embedded-tool-calls",
  ) as HTMLElement | null;
  if (!toolCallsContainer) {
    toolCallsContainer = document.createElement("div");
    toolCallsContainer.className = "embedded-tool-calls";
    toolCallsContainer.dataset.testid = "embedded-tool-calls";
    const metaBar = appendTarget.querySelector(":scope > .message-footer");
    if (metaBar) {
      appendTarget.insertBefore(toolCallsContainer, metaBar);
    } else {
      appendTarget.appendChild(toolCallsContainer);
    }
  }

  for (const toolCall of toolCalls) {
    state.toolHostByCall.set(toolCall.id, appendTarget);
    if (state.toolCards.has(toolCall.id)) continue;

    const isSpawn = isSpawnTool(toolCall.name);

    const card = document.createElement("div");
    card.className = "tool-card tool-call-item";
    if (isSpawn) card.classList.add("tool-card-spawn");
    card.dataset.testid = isSpawn ? "tool-card-spawn" : "tool-card";
    card.setAttribute("role", "region");
    card.setAttribute("aria-label", toolCall.name);

    const header = document.createElement("div");
    header.className = "tool-card-header";

    const icon = document.createElement("span");
    icon.className = "tool-status-icon";
    icon.textContent = isSpawn ? "🤖" : "⚙";

    const name = document.createElement("span");
    name.className = "tool-name";
    name.textContent = isSpawn ? "Sub-agent" : toolCall.name;

    const status = document.createElement("span");
    status.className = "tool-status-text pending";
    status.textContent = "Pending";

    const chevron = document.createElement("span");
    chevron.className = "tool-chevron";
    chevron.textContent = "▶";

    header.appendChild(icon);
    header.appendChild(name);
    header.appendChild(status);
    header.appendChild(chevron);
    card.appendChild(header);

    const details = document.createElement("div");
    details.className = "tool-details";
    card.appendChild(details);

    header.addEventListener("click", createToggleHandler(details, chevron));

    toolCallsContainer.appendChild(card);
    state.toolCards.set(toolCall.id, card);
  }

  scrollToBottom();
}

export function handleToolRequest(
  state: ToolState,
  toolCallId: string,
  toolName: string,
  toolType: ToolRequestType,
  currentBubble: HTMLElement | null,
  chatContainer: HTMLElement,
  scrollToBottom: () => void,
): void {
  const existingCard = state.toolCards.get(toolCallId);
  if (existingCard) {
    updatePendingCard(existingCard, state, toolCallId, toolName, toolType);
    scrollToBottom();
    return;
  }

  const appendTarget =
    state.toolHostByCall.get(toolCallId) ?? currentBubble ?? chatContainer;

  let toolCallsContainer = appendTarget.querySelector(
    ":scope > .embedded-tool-calls",
  ) as HTMLElement | null;
  if (!toolCallsContainer) {
    toolCallsContainer = document.createElement("div");
    toolCallsContainer.className = "embedded-tool-calls";
    toolCallsContainer.dataset.testid = "embedded-tool-calls";
    const metaBar = appendTarget.querySelector(":scope > .message-footer");
    if (metaBar) {
      appendTarget.insertBefore(toolCallsContainer, metaBar);
    } else {
      appendTarget.appendChild(toolCallsContainer);
    }
  }

  const isSpawn = isSpawnTool(toolName);

  const card = document.createElement("div");
  card.className = "tool-card tool-call-item";
  if (isSpawn) card.classList.add("tool-card-spawn");
  card.dataset.testid = isSpawn ? "tool-card-spawn" : "tool-card";
  card.setAttribute("role", "region");
  card.setAttribute("aria-label", toolName);

  const header = document.createElement("div");
  header.className = "tool-card-header";

  const icon = document.createElement("span");
  icon.className = "tool-status-icon";
  icon.textContent = isSpawn ? "🤖" : toolIcon(toolType.kind);

  const name = document.createElement("span");
  name.className = "tool-name";
  name.textContent = isSpawn ? "Sub-agent" : toolName;

  const detail = document.createElement("span");
  detail.className = "tool-header-detail";
  if (isSpawn) {
    detail.textContent = extractSpawnDetail(toolName, toolType);
  }

  const status = document.createElement("span");
  status.className = "tool-status-text";
  status.textContent = isSpawn ? "Spawned" : "Running...";

  const chevron = document.createElement("span");
  chevron.className = "tool-chevron";
  chevron.textContent = "▶";

  header.appendChild(icon);
  header.appendChild(name);
  header.appendChild(detail);
  header.appendChild(status);
  header.appendChild(chevron);
  card.appendChild(header);

  const details = document.createElement("div");
  details.className = "tool-details";
  card.appendChild(details);

  header.addEventListener("click", createToggleHandler(details, chevron));
  setCardHeaderDetail(card, toolRequestHeaderDetail(toolType), true);

  const summary = toolRequestSummary(state, toolCallId, toolName, toolType);
  if (summary) {
    details.appendChild(summary);
    setCardExpandedState(
      card,
      shouldExpandToolDetailsOnRequest(currentToolOutputMode),
    );
  }

  toolCallsContainer.appendChild(card);
  state.toolCards.set(toolCallId, card);
  state.toolHostByCall.set(toolCallId, appendTarget);
  scrollToBottom();
}

export function handleToolCompleted(
  state: ToolState,
  toolCallId: string,
  toolName: string,
  toolResult: ToolExecutionResult,
  success: boolean,
  scrollToBottom: () => void,
): boolean {
  const card = state.toolCards.get(toolCallId);
  if (!card) return false;

  const header = card.querySelector(".tool-card-header") as HTMLElement | null;
  const statusEl = card.querySelector(".tool-status-text");

  setCardHeaderDetail(card, completionHeaderDetail(card, toolResult, toolName));

  if (toolResult.kind === "ModifyFile" && header && statusEl) {
    const diffId = state.toolDiffByCall.get(toolCallId);
    if (diffId) {
      const existingBtn = header.querySelector(
        `.view-diff-btn[data-diff-id="${diffId}"]`,
      );
      if (!existingBtn) {
        const openDiff = document.createElement("button");
        openDiff.type = "button";
        openDiff.className = "view-diff-btn";
        openDiff.setAttribute("data-diff-id", diffId);
        openDiff.textContent = "⧉ Diff";
        header.insertBefore(openDiff, statusEl);
      }
    }
  }

  if (statusEl) {
    statusEl.textContent = success ? "Done" : "Failed";
    statusEl.className = `tool-status-text ${success ? "success" : "failure"}`;
  }

  const details = card.querySelector(".tool-details") as HTMLElement | null;
  const resultEl = toolResultElement(state, toolCallId, toolResult, toolName);
  if (details && resultEl) {
    details.appendChild(resultEl);
  }

  if (details) {
    setCardExpandedState(
      card,
      shouldExpandToolDetailsOnCompletion(currentToolOutputMode, success),
    );
  }

  scrollToBottom();
  return true;
}
