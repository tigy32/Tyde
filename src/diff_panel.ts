import {
  elementScroll,
  observeElementOffset,
  observeElementRect,
  Virtualizer,
} from "@tanstack/virtual-core";
import { createTwoFilesPatch } from "diff";
import hljs from "highlight.js/lib/core";
import { logTabPerf, perfNow } from "./perf_debug";
import { escapeHtml } from "./renderer";

interface DiffTab {
  id: string;
  filePath: string;
  type: "diff" | "file";
  content: string;
  diffContent?: string;
  beforeContent?: string;
  afterContent?: string;
  fullContextDiffContent?: string;
  showFullContext?: boolean;
  hunkContextExpansion?: number[];
  scrollTop?: number;
  cachedLines?: string[];
  highlightCache?: Map<number, string>;
}

type ViewMode = "unified" | "side-by-side";
const DEFAULT_DIFF_CONTEXT_LINES = 3;
const VIRTUAL_FILE_OVERSCAN_LINES = 80;
const VIRTUAL_HIGHLIGHT_BATCH_SIZE = 24;

function detectLanguage(filePath: string): string | null {
  const dot = filePath.lastIndexOf(".");
  if (dot === -1) return null;
  const ext = filePath.substring(dot).toLowerCase();
  const map: Record<string, string> = {
    ".ts": "typescript",
    ".rs": "rust",
    ".py": "python",
    ".json": "json",
    ".css": "css",
    ".html": "xml",
    ".js": "javascript",
    ".go": "go",
    ".java": "java",
    ".c": "c",
    ".cpp": "cpp",
    ".yaml": "yaml",
    ".yml": "yaml",
    ".toml": "toml",
    ".sql": "sql",
    ".md": "markdown",
    ".sh": "bash",
    ".tsx": "typescript",
    ".jsx": "javascript",
  };
  return map[ext] ?? null;
}

function highlightLine(text: string, lang: string): string {
  return hljs.highlight(text, { language: lang }).value;
}

interface ParsedHunk {
  oldStart: number;
  oldCount: number;
  newStart: number;
  newCount: number;
  lines: ParsedDiffLine[];
}

interface ParsedDiffLine {
  type: "context" | "added" | "removed" | "hunk-header" | "file-header";
  text: string;
  oldLineNum?: number;
  newLineNum?: number;
}

function isGitMetadataLine(raw: string): boolean {
  return (
    raw.startsWith("diff --git ") ||
    raw.startsWith("index ") ||
    raw.startsWith("new file mode ") ||
    raw.startsWith("deleted file mode ") ||
    raw.startsWith("old mode ") ||
    raw.startsWith("new mode ") ||
    raw.startsWith("similarity index ") ||
    raw.startsWith("rename from ") ||
    raw.startsWith("rename to ") ||
    raw.startsWith("Binary files ")
  );
}

function parseUnifiedDiff(diff: string): {
  hunks: ParsedHunk[];
  lines: ParsedDiffLine[];
} {
  const rawLines = diff.split("\n");
  const hunks: ParsedHunk[] = [];
  const allLines: ParsedDiffLine[] = [];
  let currentHunk: ParsedHunk | null = null;
  let oldLine = 0;
  let newLine = 0;

  for (const raw of rawLines) {
    if (isGitMetadataLine(raw)) {
      currentHunk = null;
      const entry: ParsedDiffLine = { type: "file-header", text: raw };
      allLines.push(entry);
      continue;
    }

    if (
      (raw.startsWith("--- ") || raw.startsWith("+++ ")) &&
      currentHunk === null
    ) {
      const entry: ParsedDiffLine = { type: "file-header", text: raw };
      allLines.push(entry);
      continue;
    }

    if (raw.startsWith("@@")) {
      const match = raw.match(/@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/);
      oldLine = match ? parseInt(match[1], 10) : 1;
      newLine = match ? parseInt(match[3], 10) : 1;
      currentHunk = {
        oldStart: oldLine,
        oldCount: match ? parseInt(match[2] ?? "1", 10) : 0,
        newStart: newLine,
        newCount: match ? parseInt(match[4] ?? "1", 10) : 0,
        lines: [],
      };
      hunks.push(currentHunk);
      const entry: ParsedDiffLine = { type: "hunk-header", text: raw };
      allLines.push(entry);
      currentHunk.lines.push(entry);
      continue;
    }

    if (raw.startsWith("\\ No newline at end of file")) {
      continue;
    }

    if (raw.startsWith("+")) {
      const entry: ParsedDiffLine = {
        type: "added",
        text: raw.substring(1),
        newLineNum: newLine++,
      };
      allLines.push(entry);
      if (currentHunk) currentHunk.lines.push(entry);
      continue;
    }

    if (raw.startsWith("-")) {
      const entry: ParsedDiffLine = {
        type: "removed",
        text: raw.substring(1),
        oldLineNum: oldLine++,
      };
      allLines.push(entry);
      if (currentHunk) currentHunk.lines.push(entry);
      continue;
    }

    const text = raw.startsWith(" ") ? raw.substring(1) : raw;
    const entry: ParsedDiffLine = {
      type: "context",
      text,
      oldLineNum: currentHunk ? oldLine++ : undefined,
      newLineNum: currentHunk ? newLine++ : undefined,
    };
    allLines.push(entry);
    if (currentHunk) currentHunk.lines.push(entry);
  }

  return { hunks, lines: allLines };
}

function getFullContextLineCount(before: string, after: string): number {
  const beforeLineCount = before.length === 0 ? 0 : before.split("\n").length;
  const afterLineCount = after.length === 0 ? 0 : after.split("\n").length;
  return Math.max(beforeLineCount, afterLineCount) + 1;
}

function generateUnifiedDiff(
  before: string,
  after: string,
  filePath: string,
  contextLines: number = DEFAULT_DIFF_CONTEXT_LINES,
): string {
  const patch = createTwoFilesPatch(
    `a/${filePath}`,
    `b/${filePath}`,
    before,
    after,
    "",
    "",
    {
      context: contextLines,
    },
  );
  let normalized = patch;
  if (
    normalized.startsWith(
      "===================================================================\n",
    )
  ) {
    normalized = normalized.substring(
      "===================================================================\n"
        .length,
    );
  }
  return normalized.endsWith("\n") ? normalized.slice(0, -1) : normalized;
}

interface FeedbackBox {
  startLine: number;
  endLine: number;
  filePath: string;
  conversationId: number | null;
  element: HTMLElement;
  status: "input" | "progress" | "complete" | "error";
  summary: string;
}

interface SearchRange {
  start: number;
  end: number;
}

interface SearchResults {
  matchLineIndexes: number[];
  rangesByLine: Map<number, SearchRange[]>;
}

interface NativeSelectionContext {
  text: string;
  filePath: string;
  startLine: number;
  endLine: number;
  selectedElements: HTMLElement[];
}

interface VirtualizedFileViewState {
  tab: DiffTab;
  viewEl: HTMLElement;
  wrapperEl: HTMLElement;
  lines: string[];
  lang: string | null;
  lineHeightPx: number;
  virtualizer: Virtualizer<HTMLElement, HTMLElement>;
  teardownVirtualizer: (() => void) | null;
  renderedStart: number;
  renderedEnd: number;
  matchOrderByLine: Map<number, number>;
  highlightRafId: number | null;
  highlightQueue: number[];
  highlightQueuedSet: Set<number>;
}

export class DiffPanel {
  private readonly abortController = new AbortController();
  private container: HTMLElement;
  private tabs: DiffTab[] = [];
  private activeTabId: string | null = null;
  private viewMode: ViewMode = "unified";
  private currentHunkIndex = -1;
  private selectionAnchor: number | null = null;
  private selectionEnd: number | null = null;
  private selectionComplete = false;
  private feedbackBoxes: Map<string, FeedbackBox> = new Map();
  private fileSearchOpen = false;
  private fileSearchQuery = "";
  private fileSearchCaseSensitive = false;
  private fileSearchWholeWord = false;
  private fileSearchRegex = false;
  private fileSearchError: string | null = null;
  private fileSearchMatchLineIndexes: number[] = [];
  private fileSearchMatchRangesByLine: Map<number, SearchRange[]> = new Map();
  private fileSearchActiveIndex = -1;
  private pendingFindFocus = false;
  private pendingFindSelectAll = false;
  private pendingScrollToSearchMatch = false;
  private fileCopyStatus: "idle" | "success" | "error" = "idle";
  private fileCopyResetTimer: number | null = null;
  private fileGoToLineOpen = false;
  private fileGoToLineValue = "";
  private pendingGoToLineFocus = false;
  private pendingGoToLineSelectAll = false;
  private pendingGoToLineTarget: number | null = null;
  private fileGoToLineFlashIndex: number | null = null;
  private fileGoToLineFlashTimer: number | null = null;
  private fileWordWrap = false;
  private nativeSelectionTimerId: number | null = null;
  private virtualizedFileView: VirtualizedFileViewState | null = null;

  onViewDiff: ((filePath: string, diffContent: string) => void) | null = null;
  onFeedbackSubmit:
    | ((
        filePath: string,
        startLine: number,
        endLine: number,
        lineContent: string,
        feedback: string,
      ) => Promise<number>)
    | null = null;

  constructor(container: HTMLElement) {
    this.container = container;
    this.container.addEventListener("copy", (e) =>
      this.handleFileSelectionCopy(e as ClipboardEvent),
    );
    document.addEventListener(
      "selectionchange",
      () => this.scheduleNativeSelectionSync(),
      { signal: this.abortController.signal },
    );
  }

  dispose(): void {
    this.abortController.abort();
  }

  focusFind(): boolean {
    const tab = this.getActiveTab();
    if (!tab || tab.type !== "file") return false;

    this.fileSearchOpen = true;
    this.pendingFindFocus = true;
    this.pendingFindSelectAll = true;
    this.renderPreservingScroll();
    return true;
  }

  focusGoToLine(): boolean {
    const tab = this.getActiveTab();
    if (!tab || tab.type !== "file") return false;

    this.fileGoToLineOpen = true;
    this.pendingGoToLineFocus = true;
    this.pendingGoToLineSelectAll = true;
    this.renderPreservingScroll();
    return true;
  }

  revealFileLine(tabId: string, oneBasedLine: number): boolean {
    const tab = this.tabs.find((t) => t.id === tabId && t.type === "file");
    if (!tab) return false;

    const lines = this.getFileLines(tab);
    if (lines.length === 0) return false;

    const parsedLine = Number.isFinite(oneBasedLine)
      ? Math.trunc(oneBasedLine)
      : 1;
    const clampedLine = Math.max(1, Math.min(lines.length, parsedLine));
    const lineIndex = clampedLine - 1;

    if (this.activeTabId !== tab.id) {
      this.switchTab(tab.id);
    }

    this.pendingGoToLineTarget = lineIndex;
    this.fileGoToLineFlashIndex = lineIndex;
    this.render();
    this.scheduleGoToLineFlashReset();
    return true;
  }

  showBeforeAfterDiff(
    before: string,
    after: string,
    filePath: string,
    preferredTabId?: string,
  ): string {
    const diff = generateUnifiedDiff(before, after, filePath);
    const existing = this.tabs.find(
      (t) =>
        (preferredTabId ? t.id === preferredTabId : false) ||
        (t.filePath === filePath && t.type === "diff"),
    );
    if (existing) {
      existing.diffContent = diff;
      existing.beforeContent = before;
      existing.afterContent = after;
      existing.fullContextDiffContent = undefined;
      existing.hunkContextExpansion = [];
      this.setDiffContextMode(existing, existing.showFullContext === true);
      this.switchTab(existing.id);
      return existing.id;
    }

    const tab: DiffTab = {
      id:
        preferredTabId ??
        `diff-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`,
      filePath,
      type: "diff",
      content: diff,
      diffContent: diff,
      beforeContent: before,
      afterContent: after,
      showFullContext: false,
      hunkContextExpansion: [],
    };
    this.addTab(tab);
    return tab.id;
  }

  showUnifiedDiff(
    diff: string,
    filePath: string,
    preferredTabId?: string,
  ): string {
    const existing = this.tabs.find(
      (t) =>
        (preferredTabId ? t.id === preferredTabId : false) ||
        (t.filePath === filePath && t.type === "diff"),
    );
    if (existing) {
      existing.content = diff;
      existing.diffContent = diff;
      existing.beforeContent = undefined;
      existing.afterContent = undefined;
      existing.fullContextDiffContent = undefined;
      existing.showFullContext = false;
      existing.hunkContextExpansion = [];
      this.switchTab(existing.id);
      return existing.id;
    }

    const tab: DiffTab = {
      id:
        preferredTabId ??
        `diff-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`,
      filePath,
      type: "diff",
      content: diff,
      diffContent: diff,
      showFullContext: false,
      hunkContextExpansion: [],
    };
    this.addTab(tab);
    return tab.id;
  }

  private canShowFullDiffContext(tab: DiffTab): boolean {
    return (
      tab.type === "diff" &&
      tab.beforeContent !== undefined &&
      tab.afterContent !== undefined
    );
  }

  private getFullContextDiff(tab: DiffTab): string {
    if (tab.fullContextDiffContent !== undefined) {
      return tab.fullContextDiffContent;
    }

    if (tab.beforeContent === undefined || tab.afterContent === undefined) {
      return tab.diffContent ?? tab.content;
    }

    const contextLines = getFullContextLineCount(
      tab.beforeContent,
      tab.afterContent,
    );
    tab.fullContextDiffContent = generateUnifiedDiff(
      tab.beforeContent,
      tab.afterContent,
      tab.filePath,
      contextLines,
    );
    return tab.fullContextDiffContent;
  }

  private setDiffContextMode(tab: DiffTab, showFullContext: boolean): void {
    if (tab.type !== "diff") return;

    if (!this.canShowFullDiffContext(tab)) {
      tab.showFullContext = false;
      tab.content = tab.diffContent ?? tab.content;
      return;
    }

    tab.showFullContext = showFullContext;
    tab.content = showFullContext
      ? this.getFullContextDiff(tab)
      : (tab.diffContent ?? tab.content);
  }

  private canExpandInlineHunkContext(tab: DiffTab): boolean {
    return (
      tab.type === "diff" &&
      tab.showFullContext !== true &&
      this.canShowFullDiffContext(tab)
    );
  }

  private getHunkContextExpansion(tab: DiffTab, hunkIndex: number): number {
    return tab.hunkContextExpansion?.[hunkIndex] ?? 0;
  }

  private expandHunkContext(tab: DiffTab, hunkIndex: number): void {
    if (!this.canExpandInlineHunkContext(tab)) return;
    if (!tab.hunkContextExpansion) tab.hunkContextExpansion = [];
    tab.hunkContextExpansion[hunkIndex] =
      (tab.hunkContextExpansion[hunkIndex] ?? 0) + 10;
    this.renderPreservingScroll();
  }

  private getRenderedDiff(tab: DiffTab): {
    hunks: ParsedHunk[];
    lines: ParsedDiffLine[];
  } {
    if (!this.canExpandInlineHunkContext(tab)) {
      return parseUnifiedDiff(tab.content);
    }

    const baseDiff = tab.diffContent ?? tab.content;
    const expansions = tab.hunkContextExpansion ?? [];
    const hasExpansion = expansions.some((value) => value > 0);
    if (!hasExpansion) {
      return parseUnifiedDiff(baseDiff);
    }

    return this.buildExpandedHunkDiff(tab, expansions);
  }

  private buildExpandedHunkDiff(
    tab: DiffTab,
    expansions: number[],
  ): { hunks: ParsedHunk[]; lines: ParsedDiffLine[] } {
    const baseDiff = tab.diffContent ?? tab.content;
    const baseParsed = parseUnifiedDiff(baseDiff);
    if (
      tab.beforeContent === undefined ||
      tab.afterContent === undefined ||
      baseParsed.hunks.length === 0
    ) {
      return baseParsed;
    }

    const beforeLines =
      tab.beforeContent.length === 0 ? [] : tab.beforeContent.split("\n");
    const afterLines =
      tab.afterContent.length === 0 ? [] : tab.afterContent.split("\n");
    const hunks: ParsedHunk[] = [];
    const lines: ParsedDiffLine[] = [];

    const formatRange = (start: number, count: number): string => {
      if (count === 1) return String(start);
      return `${start},${count}`;
    };

    for (let hunkIndex = 0; hunkIndex < baseParsed.hunks.length; hunkIndex++) {
      const baseHunk = baseParsed.hunks[hunkIndex];
      const extra = Math.max(0, expansions[hunkIndex] ?? 0);
      const baseBodyLines = baseHunk.lines
        .filter((line) => line.type !== "hunk-header")
        .map((line) => ({ ...line }));

      const topContextLines: ParsedDiffLine[] = [];
      if (extra > 0) {
        for (let offset = extra; offset >= 1; offset--) {
          const oldLineNum = baseHunk.oldStart - offset;
          const newLineNum = baseHunk.newStart - offset;
          const oldValid = oldLineNum >= 1 && oldLineNum <= beforeLines.length;
          const newValid = newLineNum >= 1 && newLineNum <= afterLines.length;
          if (!oldValid && !newValid) continue;
          topContextLines.push({
            type: "context",
            text: oldValid
              ? beforeLines[oldLineNum - 1]
              : afterLines[newLineNum - 1],
            oldLineNum: oldValid ? oldLineNum : undefined,
            newLineNum: newValid ? newLineNum : undefined,
          });
        }
      }

      const oldEndBase =
        baseHunk.oldCount > 0
          ? baseHunk.oldStart + baseHunk.oldCount - 1
          : baseHunk.oldStart - 1;
      const newEndBase =
        baseHunk.newCount > 0
          ? baseHunk.newStart + baseHunk.newCount - 1
          : baseHunk.newStart - 1;

      const bottomContextLines: ParsedDiffLine[] = [];
      if (extra > 0) {
        for (let offset = 1; offset <= extra; offset++) {
          const oldLineNum = oldEndBase + offset;
          const newLineNum = newEndBase + offset;
          const oldValid = oldLineNum >= 1 && oldLineNum <= beforeLines.length;
          const newValid = newLineNum >= 1 && newLineNum <= afterLines.length;
          if (!oldValid && !newValid) continue;
          bottomContextLines.push({
            type: "context",
            text: oldValid
              ? beforeLines[oldLineNum - 1]
              : afterLines[newLineNum - 1],
            oldLineNum: oldValid ? oldLineNum : undefined,
            newLineNum: newValid ? newLineNum : undefined,
          });
        }
      }

      const expandedBody = [
        ...topContextLines,
        ...baseBodyLines,
        ...bottomContextLines,
      ];
      const oldLineNumbers = expandedBody
        .map((line) => line.oldLineNum)
        .filter((lineNum): lineNum is number => lineNum !== undefined);
      const newLineNumbers = expandedBody
        .map((line) => line.newLineNum)
        .filter((lineNum): lineNum is number => lineNum !== undefined);

      const oldStart =
        oldLineNumbers.length > 0
          ? oldLineNumbers[0]
          : Math.max(0, baseHunk.oldStart - extra);
      const newStart =
        newLineNumbers.length > 0
          ? newLineNumbers[0]
          : Math.max(0, baseHunk.newStart - extra);
      const oldCount = oldLineNumbers.length;
      const newCount = newLineNumbers.length;

      const headerLine: ParsedDiffLine = {
        type: "hunk-header",
        text: `@@ -${formatRange(oldStart, oldCount)} +${formatRange(newStart, newCount)} @@`,
      };

      const hunkLines = [headerLine, ...expandedBody];
      hunks.push({
        oldStart,
        oldCount,
        newStart,
        newCount,
        lines: hunkLines,
      });
      lines.push(...hunkLines);
    }

    return { hunks, lines };
  }

  showFileContent(
    content: string,
    filePath: string,
    preferredTabId?: string,
  ): string {
    const existing = this.tabs.find(
      (t) =>
        (preferredTabId ? t.id === preferredTabId : false) ||
        (t.filePath === filePath && t.type === "file"),
    );
    if (existing) {
      existing.content = content;
      this.invalidateFileCaches(existing);
      this.switchTab(existing.id);
      return existing.id;
    }

    const tab: DiffTab = {
      id:
        preferredTabId ??
        `file-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`,
      filePath,
      type: "file",
      content,
    };
    this.addTab(tab);
    return tab.id;
  }

  activateTab(id: string): boolean {
    const exists = this.tabs.some((t) => t.id === id);
    if (!exists) return false;
    this.switchTab(id);
    return true;
  }

  closeTabById(id: string): void {
    this.closeTab(id);
  }

  clear(): void {
    this.tabs = [];
    this.activeTabId = null;
    this.currentHunkIndex = -1;
    this.feedbackBoxes.clear();
    this.selectionAnchor = null;
    this.selectionEnd = null;
    this.selectionComplete = false;

    this.fileSearchQuery = "";
    this.fileSearchCaseSensitive = false;
    this.fileSearchWholeWord = false;
    this.fileSearchRegex = false;
    this.fileSearchError = null;
    this.fileSearchMatchLineIndexes = [];
    this.fileSearchMatchRangesByLine.clear();
    this.fileSearchActiveIndex = -1;
    this.fileSearchOpen = false;
    this.pendingFindFocus = false;
    this.pendingFindSelectAll = false;
    this.pendingScrollToSearchMatch = false;
    this.fileCopyStatus = "idle";
    if (this.fileCopyResetTimer !== null) {
      window.clearTimeout(this.fileCopyResetTimer);
      this.fileCopyResetTimer = null;
    }
    this.fileGoToLineOpen = false;
    this.fileGoToLineValue = "";
    this.pendingGoToLineFocus = false;
    this.pendingGoToLineSelectAll = false;
    this.pendingGoToLineTarget = null;
    this.fileGoToLineFlashIndex = null;
    if (this.fileGoToLineFlashTimer !== null) {
      window.clearTimeout(this.fileGoToLineFlashTimer);
      this.fileGoToLineFlashTimer = null;
    }
    if (this.nativeSelectionTimerId !== null) {
      window.clearTimeout(this.nativeSelectionTimerId);
      this.nativeSelectionTimerId = null;
    }
    this.fileWordWrap = false;
    this.render();
  }

  showLoading(): void {
    this.container.innerHTML = "";
    const loading = document.createElement("div");
    loading.className = "panel-loading";
    const spinner = document.createElement("span");
    spinner.className = "loading-spinner";
    loading.appendChild(spinner);
    loading.appendChild(document.createTextNode("Loading diff..."));
    this.container.appendChild(loading);
  }

  private addTab(tab: DiffTab): void {
    this.tabs.push(tab);
    this.switchTab(tab.id);
  }

  private switchTab(id: string): void {
    const start = perfNow();
    const saveStart = perfNow();
    this.saveScrollPosition();
    const saveMs = perfNow() - saveStart;
    this.activeTabId = id;
    this.currentHunkIndex = -1;
    const renderStart = perfNow();
    this.render();
    const renderMs = perfNow() - renderStart;
    const restoreStart = perfNow();
    this.restoreScrollPosition();
    const restoreMs = perfNow() - restoreStart;
    const active = this.getActiveTab();
    logTabPerf("DiffPanel.switchTab", perfNow() - start, {
      tabId: id,
      tabType: active?.type ?? "unknown",
      saveMs,
      renderMs,
      restoreMs,
    });
  }

  private closeTab(id: string): void {
    const idx = this.tabs.findIndex((t) => t.id === id);
    if (idx === -1) return;

    this.tabs.splice(idx, 1);

    if (this.activeTabId === id) {
      if (this.tabs.length === 0) {
        this.activeTabId = null;
      } else {
        const newIdx = Math.min(idx, this.tabs.length - 1);
        this.activeTabId = this.tabs[newIdx].id;
      }
    }

    this.currentHunkIndex = -1;
    this.render();
  }

  private findScrollContainer(): HTMLElement | null {
    return (this.container.querySelector(".diff-panel-content-view") ??
      this.container.querySelector(
        ".diff-panel-sbs-left",
      )) as HTMLElement | null;
  }

  private saveScrollPosition(): void {
    if (!this.activeTabId) return;
    const tab = this.tabs.find((t) => t.id === this.activeTabId);
    if (!tab) return;
    const viewEl = this.findScrollContainer();
    if (viewEl) tab.scrollTop = viewEl.scrollTop;
  }

  private restoreScrollPosition(): void {
    if (!this.activeTabId) return;
    const tab = this.tabs.find((t) => t.id === this.activeTabId);
    if (!tab || tab.scrollTop === undefined) return;
    const viewEl = this.findScrollContainer();
    if (viewEl) viewEl.scrollTop = tab.scrollTop;
  }

  private renderPreservingScroll(): void {
    this.saveScrollPosition();
    this.render();
    this.restoreScrollPosition();
  }

  private getActiveTab(): DiffTab | null {
    if (!this.activeTabId) return null;
    return this.tabs.find((t) => t.id === this.activeTabId) ?? null;
  }

  private getFileLines(tab: DiffTab): string[] {
    if (!tab.cachedLines) {
      tab.cachedLines = tab.content.split("\n");
    }
    return tab.cachedLines;
  }

  private invalidateFileCaches(tab: DiffTab): void {
    tab.cachedLines = undefined;
    tab.highlightCache = undefined;
  }

  private getHighlightCache(tab: DiffTab): Map<number, string> {
    if (!tab.highlightCache) {
      tab.highlightCache = new Map<number, string>();
    }
    return tab.highlightCache;
  }

  private shouldVirtualizeFileContent(): boolean {
    return !this.fileWordWrap;
  }

  private disposeVirtualizedFileView(): void {
    if (!this.virtualizedFileView) return;
    const state = this.virtualizedFileView;
    state.teardownVirtualizer?.();
    if (state.highlightRafId !== null) {
      window.cancelAnimationFrame(state.highlightRafId);
    }
    this.virtualizedFileView = null;
  }

  private render(): void {
    this.disposeVirtualizedFileView();
    this.container.innerHTML = "";
    if (this.tabs.length === 0) {
      this.renderEmptyState();
      return;
    }

    if (!this.activeTabId) {
      this.activeTabId = this.tabs[0].id;
    }

    const activeTab = this.getActiveTab();
    if (!activeTab) {
      this.renderEmptyState();
      return;
    }

    this.container.appendChild(this.renderContentHeader(activeTab));

    if (activeTab.type === "diff") {
      this.container.appendChild(this.renderDiffContent(activeTab));
    } else {
      this.container.appendChild(this.renderFileContent(activeTab));
    }

    this.applyPostRenderActions(activeTab);
    this.syncNativeSelectionHighlight();
  }

  private renderEmptyState(): void {
    const empty = document.createElement("div");
    empty.className = "panel-empty-state";

    const icon = document.createElement("span");
    icon.className = "panel-empty-state-icon";
    icon.textContent = "⇄";

    const label = document.createElement("span");
    label.textContent = "No diff selected";

    empty.appendChild(icon);
    empty.appendChild(label);
    this.container.appendChild(empty);
  }

  private renderContentHeader(tab: DiffTab): HTMLElement {
    const header = document.createElement("div");
    header.className = "diff-panel-content-header";

    const pathEl = document.createElement("span");
    pathEl.className = "diff-panel-filepath";
    pathEl.textContent = tab.filePath;
    header.appendChild(pathEl);

    if (tab.type === "file") {
      header.appendChild(this.renderFileToolbar(tab));
      return header;
    }

    const controls = document.createElement("div");
    controls.className = "diff-panel-controls";

    const toggleBtn = document.createElement("button");
    toggleBtn.className = "diff-panel-toggle-btn";
    toggleBtn.textContent =
      this.viewMode === "unified" ? "⫿ Side-by-Side" : "≡ Unified";
    toggleBtn.title = `Switch to ${this.viewMode === "unified" ? "side-by-side" : "unified"} view`;
    toggleBtn.addEventListener("click", () => {
      this.saveScrollPosition();
      this.viewMode = this.viewMode === "unified" ? "side-by-side" : "unified";
      this.render();
    });
    controls.appendChild(toggleBtn);

    const canShowFullContext = this.canShowFullDiffContext(tab);
    const fullContextBtn = document.createElement("button");
    fullContextBtn.className = "diff-panel-toggle-btn";
    if (tab.showFullContext) fullContextBtn.classList.add("active");
    fullContextBtn.textContent = tab.showFullContext
      ? "Hunks Only"
      : "Full Context";
    fullContextBtn.title = canShowFullContext
      ? tab.showFullContext
        ? "Collapse to hunk-only diff context"
        : "Expand to full diff context"
      : "Full context is unavailable for this diff";
    fullContextBtn.disabled = !canShowFullContext;
    fullContextBtn.addEventListener("click", () => {
      this.saveScrollPosition();
      this.setDiffContextMode(tab, !(tab.showFullContext === true));
      this.render();
    });
    controls.appendChild(fullContextBtn);

    const parsed = this.getRenderedDiff(tab);
    const hunkCount = parsed.hunks.length;

    if (hunkCount > 0) {
      const prevBtn = document.createElement("button");
      prevBtn.className = "diff-panel-nav-btn";
      prevBtn.textContent = "↑";
      prevBtn.title = "Previous change";
      prevBtn.addEventListener("click", () => this.navigateHunk(-1));

      const nextBtn = document.createElement("button");
      nextBtn.className = "diff-panel-nav-btn";
      nextBtn.textContent = "↓";
      nextBtn.title = "Next change";
      nextBtn.addEventListener("click", () => this.navigateHunk(1));

      const indicator = document.createElement("span");
      indicator.className = "diff-panel-hunk-indicator";
      if (this.currentHunkIndex >= 0) {
        indicator.textContent = `Change ${this.currentHunkIndex + 1} of ${hunkCount}`;
      } else {
        indicator.textContent = `${hunkCount} change${hunkCount !== 1 ? "s" : ""}`;
      }

      controls.appendChild(prevBtn);
      controls.appendChild(indicator);
      controls.appendChild(nextBtn);
    }

    header.appendChild(controls);
    return header;
  }

  private renderFileToolbar(tab: DiffTab): HTMLElement {
    const lines = this.getFileLines(tab);
    this.updateFileSearchResults(lines);

    const controls = document.createElement("div");
    controls.className = "diff-panel-controls diff-panel-file-toolbar";

    if (this.fileGoToLineOpen) {
      const goWrap = document.createElement("div");
      goWrap.className = "diff-panel-goto-wrap";

      const goLabel = document.createElement("span");
      goLabel.className = "diff-panel-goto-label";
      goLabel.textContent = "Line";
      goWrap.appendChild(goLabel);

      const goInput = document.createElement("input");
      goInput.type = "text";
      goInput.inputMode = "numeric";
      goInput.className = "diff-panel-goto-input";
      goInput.placeholder = `1-${lines.length}`;
      goInput.value = this.fileGoToLineValue;
      goInput.setAttribute("aria-label", "Go to line");
      goInput.addEventListener("input", () => {
        this.fileGoToLineValue = goInput.value.replace(/[^\d]/g, "");
      });
      goInput.addEventListener("keydown", (e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          this.navigateToLineFromInput(lines.length);
          return;
        }
        if (e.key === "Escape") {
          e.preventDefault();
          this.closeGoToLine();
        }
      });
      goWrap.appendChild(goInput);

      const goBtn = document.createElement("button");
      goBtn.type = "button";
      goBtn.className = "diff-panel-nav-btn";
      goBtn.title = "Go to line";
      goBtn.setAttribute("aria-label", "Go to line");
      goBtn.textContent = "→";
      goBtn.addEventListener("click", () =>
        this.navigateToLineFromInput(lines.length),
      );
      goWrap.appendChild(goBtn);

      const closeGoBtn = document.createElement("button");
      closeGoBtn.type = "button";
      closeGoBtn.className = "diff-panel-icon-btn";
      closeGoBtn.title = "Close go to line";
      closeGoBtn.setAttribute("aria-label", "Close go to line");
      closeGoBtn.textContent = "×";
      closeGoBtn.addEventListener("click", () => this.closeGoToLine());
      goWrap.appendChild(closeGoBtn);

      controls.appendChild(goWrap);
    }

    if (this.fileSearchOpen) {
      const findWrap = document.createElement("div");
      findWrap.className = "diff-panel-find-wrap";

      const input = document.createElement("input");
      input.type = "text";
      input.className = "diff-panel-find-input";
      input.placeholder = "Find in file";
      input.value = this.fileSearchQuery;
      input.setAttribute("aria-label", "Find in file");
      input.addEventListener("input", () =>
        this.updateFileSearchQuery(input.value),
      );
      input.addEventListener("keydown", (e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          this.navigateFileSearch(e.shiftKey ? -1 : 1);
          return;
        }
        if (e.key === "Escape") {
          e.preventDefault();
          this.closeFileSearch();
        }
      });
      findWrap.appendChild(input);

      const caseBtn = document.createElement("button");
      caseBtn.type = "button";
      caseBtn.className = "diff-panel-find-toggle-btn";
      if (this.fileSearchCaseSensitive) caseBtn.classList.add("active");
      caseBtn.title = "Match case";
      caseBtn.setAttribute("aria-label", "Match case");
      caseBtn.textContent = "Aa";
      caseBtn.addEventListener("click", () =>
        this.toggleFileSearchOption("case"),
      );
      findWrap.appendChild(caseBtn);

      const wholeBtn = document.createElement("button");
      wholeBtn.type = "button";
      wholeBtn.className = "diff-panel-find-toggle-btn";
      if (this.fileSearchWholeWord) wholeBtn.classList.add("active");
      wholeBtn.title = "Whole word";
      wholeBtn.setAttribute("aria-label", "Whole word");
      wholeBtn.textContent = "W";
      wholeBtn.addEventListener("click", () =>
        this.toggleFileSearchOption("whole"),
      );
      findWrap.appendChild(wholeBtn);

      const regexBtn = document.createElement("button");
      regexBtn.type = "button";
      regexBtn.className = "diff-panel-find-toggle-btn";
      if (this.fileSearchRegex) regexBtn.classList.add("active");
      regexBtn.title = "Regex";
      regexBtn.setAttribute("aria-label", "Regex");
      regexBtn.textContent = ".*";
      regexBtn.addEventListener("click", () =>
        this.toggleFileSearchOption("regex"),
      );
      findWrap.appendChild(regexBtn);

      const count = document.createElement("span");
      count.className = "diff-panel-find-count";
      if (this.fileSearchError) {
        count.classList.add("diff-panel-find-count-error");
        count.title = this.fileSearchError;
        count.textContent = "ERR";
      } else if (
        this.fileSearchMatchLineIndexes.length > 0 &&
        this.fileSearchActiveIndex >= 0
      ) {
        count.textContent = `${this.fileSearchActiveIndex + 1}/${this.fileSearchMatchLineIndexes.length}`;
      } else if (this.fileSearchQuery.trim()) {
        count.textContent = "0/0";
      } else {
        count.textContent = "";
      }
      findWrap.appendChild(count);

      const prevBtn = document.createElement("button");
      prevBtn.type = "button";
      prevBtn.className = "diff-panel-nav-btn";
      prevBtn.title = "Previous match";
      prevBtn.setAttribute("aria-label", "Previous match");
      prevBtn.textContent = "↑";
      prevBtn.addEventListener("click", () => this.navigateFileSearch(-1));
      if (
        this.fileSearchMatchLineIndexes.length === 0 ||
        this.fileSearchError !== null
      )
        prevBtn.disabled = true;
      findWrap.appendChild(prevBtn);

      const nextBtn = document.createElement("button");
      nextBtn.type = "button";
      nextBtn.className = "diff-panel-nav-btn";
      nextBtn.title = "Next match";
      nextBtn.setAttribute("aria-label", "Next match");
      nextBtn.textContent = "↓";
      nextBtn.addEventListener("click", () => this.navigateFileSearch(1));
      if (
        this.fileSearchMatchLineIndexes.length === 0 ||
        this.fileSearchError !== null
      )
        nextBtn.disabled = true;
      findWrap.appendChild(nextBtn);

      const closeBtn = document.createElement("button");
      closeBtn.type = "button";
      closeBtn.className = "diff-panel-icon-btn";
      closeBtn.title = "Close find";
      closeBtn.setAttribute("aria-label", "Close find");
      closeBtn.textContent = "×";
      closeBtn.addEventListener("click", () => this.closeFileSearch());
      findWrap.appendChild(closeBtn);

      controls.appendChild(findWrap);
    }

    const actions = document.createElement("div");
    actions.className = "diff-panel-file-actions";

    const copyBtn = document.createElement("button");
    copyBtn.type = "button";
    copyBtn.className = "diff-panel-icon-btn";
    copyBtn.title = "Copy full file";
    copyBtn.setAttribute("aria-label", "Copy full file");
    copyBtn.textContent =
      this.fileCopyStatus === "success"
        ? "✓"
        : this.fileCopyStatus === "error"
          ? "!"
          : "⧉";
    copyBtn.addEventListener("click", () => {
      void this.copyEntireFile(tab);
    });
    actions.appendChild(copyBtn);

    const goToBtn = document.createElement("button");
    goToBtn.type = "button";
    goToBtn.className = "diff-panel-icon-btn";
    if (this.fileGoToLineOpen) goToBtn.classList.add("active");
    goToBtn.title = "Go to line (Ctrl/Cmd+G)";
    goToBtn.setAttribute("aria-label", "Go to line");
    goToBtn.textContent = "#";
    goToBtn.addEventListener("click", () => {
      this.fileGoToLineOpen = !this.fileGoToLineOpen;
      this.pendingGoToLineFocus = this.fileGoToLineOpen;
      this.pendingGoToLineSelectAll = this.fileGoToLineOpen;
      this.renderPreservingScroll();
    });
    actions.appendChild(goToBtn);

    const wrapBtn = document.createElement("button");
    wrapBtn.type = "button";
    wrapBtn.className = "diff-panel-icon-btn";
    if (this.fileWordWrap) wrapBtn.classList.add("active");
    wrapBtn.title = this.fileWordWrap
      ? "Disable word wrap"
      : "Enable word wrap";
    wrapBtn.setAttribute("aria-label", "Toggle word wrap");
    wrapBtn.textContent = "↩";
    wrapBtn.addEventListener("click", () => {
      this.fileWordWrap = !this.fileWordWrap;
      this.renderPreservingScroll();
    });
    actions.appendChild(wrapBtn);

    const searchBtn = document.createElement("button");
    searchBtn.type = "button";
    searchBtn.className = "diff-panel-icon-btn";
    if (this.fileSearchOpen) searchBtn.classList.add("active");
    searchBtn.title = "Find in file (Ctrl/Cmd+F)";
    searchBtn.setAttribute("aria-label", "Find in file");
    searchBtn.textContent = "⌕";
    searchBtn.addEventListener("click", () => {
      this.fileSearchOpen = true;
      this.pendingFindFocus = true;
      this.pendingFindSelectAll = true;
      this.renderPreservingScroll();
    });
    actions.appendChild(searchBtn);

    controls.appendChild(actions);
    return controls;
  }

  private scrollToFileLine(
    lineIndex: number,
    behavior: "auto" | "smooth",
  ): void {
    if (lineIndex < 0) return;
    const virtual = this.virtualizedFileView;
    if (virtual && this.activeTabId === virtual.tab.id) {
      virtual.virtualizer.scrollToIndex(lineIndex, {
        align: "center",
        behavior,
      });
      this.updateVirtualizedFileViewport(virtual, true);
      return;
    }

    const lineEl = this.container.querySelector<HTMLElement>(
      `.diff-panel-file-line[data-line-index="${lineIndex}"]`,
    );
    lineEl?.scrollIntoView({ behavior, block: "center" });
  }

  private applyPostRenderActions(tab: DiffTab): void {
    if (tab.type !== "file") {
      this.pendingFindFocus = false;
      this.pendingFindSelectAll = false;
      this.pendingGoToLineFocus = false;
      this.pendingGoToLineSelectAll = false;
      this.pendingGoToLineTarget = null;
      this.pendingScrollToSearchMatch = false;
      return;
    }

    if (this.pendingGoToLineFocus) {
      const goInput = this.container.querySelector<HTMLInputElement>(
        ".diff-panel-goto-input",
      );
      if (goInput) {
        goInput.focus();
        if (this.pendingGoToLineSelectAll) {
          goInput.select();
        }
      }
      this.pendingGoToLineFocus = false;
      this.pendingGoToLineSelectAll = false;
    }

    if (this.pendingFindFocus) {
      const input = this.container.querySelector<HTMLInputElement>(
        ".diff-panel-find-input",
      );
      if (input) {
        input.focus();
        if (this.pendingFindSelectAll) {
          input.select();
        }
      }
      this.pendingFindFocus = false;
      this.pendingFindSelectAll = false;
    }

    if (this.pendingGoToLineTarget !== null) {
      this.scrollToFileLine(this.pendingGoToLineTarget, "smooth");
      this.pendingGoToLineTarget = null;
    }

    if (this.pendingScrollToSearchMatch && this.fileSearchActiveIndex >= 0) {
      const lineIndex =
        this.fileSearchMatchLineIndexes[this.fileSearchActiveIndex];
      if (lineIndex !== undefined) {
        this.scrollToFileLine(lineIndex, "smooth");
      }
    }
    this.pendingScrollToSearchMatch = false;
  }

  private updateFileSearchQuery(query: string): void {
    this.fileSearchQuery = query;

    if (!query.trim()) {
      this.resetSearchResults();
      this.pendingFindFocus = true;
      this.pendingFindSelectAll = false;
      this.pendingScrollToSearchMatch = false;
      this.renderPreservingScroll();
      return;
    }

    this.fileSearchActiveIndex = 0;
    this.pendingFindFocus = true;
    this.pendingFindSelectAll = false;
    this.pendingScrollToSearchMatch = true;
    this.render();
  }

  private navigateFileSearch(direction: 1 | -1): void {
    const tab = this.getActiveTab();
    if (!tab || tab.type !== "file") return;
    this.updateFileSearchResults(this.getFileLines(tab));
    if (this.fileSearchError) return;
    const matches = this.fileSearchMatchLineIndexes;
    if (matches.length === 0) return;

    if (
      this.fileSearchActiveIndex < 0 ||
      this.fileSearchActiveIndex >= matches.length
    ) {
      this.fileSearchActiveIndex = direction > 0 ? 0 : matches.length - 1;
    } else {
      this.fileSearchActiveIndex =
        (this.fileSearchActiveIndex + direction + matches.length) %
        matches.length;
    }

    const lineIndex = matches[this.fileSearchActiveIndex];
    if (lineIndex === undefined) return;

    // Update counter text in-place
    const countEl = this.container.querySelector<HTMLElement>(
      ".diff-panel-find-count",
    );
    if (countEl) {
      countEl.textContent = `${this.fileSearchActiveIndex + 1}/${matches.length}`;
    }

    // Swap active class in-place for non-virtualized views.
    // For virtualized views, scrollToFileLine triggers a forced viewport
    // re-render which applies the class via createFileLineElement.
    if (!this.virtualizedFileView) {
      const oldActive = this.container.querySelector(
        ".diff-panel-search-hit-active",
      );
      if (oldActive) oldActive.classList.remove("diff-panel-search-hit-active");
      const newActive = this.container.querySelector(
        `[data-search-match-index="${this.fileSearchActiveIndex}"]`,
      );
      if (newActive) newActive.classList.add("diff-panel-search-hit-active");
    }

    this.scrollToFileLine(lineIndex, "smooth");

    // Re-focus the search input
    const input = this.container.querySelector<HTMLInputElement>(
      ".diff-panel-find-input",
    );
    if (input) input.focus();
  }

  private closeFileSearch(): void {
    this.fileSearchOpen = false;
    this.fileSearchQuery = "";
    this.resetSearchResults();
    this.pendingFindFocus = false;
    this.pendingFindSelectAll = false;
    this.pendingScrollToSearchMatch = false;
    this.renderPreservingScroll();
  }

  private closeGoToLine(): void {
    this.fileGoToLineOpen = false;
    this.fileGoToLineValue = "";
    this.pendingGoToLineFocus = false;
    this.pendingGoToLineSelectAll = false;
    this.pendingGoToLineTarget = null;
    this.renderPreservingScroll();
  }

  private navigateToLineFromInput(totalLines: number): void {
    const parsed = Number.parseInt(this.fileGoToLineValue, 10);
    if (Number.isNaN(parsed) || totalLines <= 0) return;
    const clamped = Math.max(1, Math.min(totalLines, parsed)) - 1;
    this.pendingGoToLineTarget = clamped;
    this.fileGoToLineFlashIndex = clamped;
    this.render();
    this.scheduleGoToLineFlashReset();
  }

  private scheduleGoToLineFlashReset(): void {
    if (this.fileGoToLineFlashTimer !== null) {
      window.clearTimeout(this.fileGoToLineFlashTimer);
    }
    this.fileGoToLineFlashTimer = window.setTimeout(() => {
      this.fileGoToLineFlashTimer = null;
      this.fileGoToLineFlashIndex = null;
      this.renderPreservingScroll();
    }, 1200);
  }

  private toggleFileSearchOption(option: "case" | "whole" | "regex"): void {
    if (option === "case")
      this.fileSearchCaseSensitive = !this.fileSearchCaseSensitive;
    if (option === "whole")
      this.fileSearchWholeWord = !this.fileSearchWholeWord;
    if (option === "regex") this.fileSearchRegex = !this.fileSearchRegex;

    this.pendingFindFocus = true;
    this.pendingFindSelectAll = false;
    this.pendingScrollToSearchMatch = true;
    this.render();
  }

  private resetSearchResults(): void {
    this.fileSearchMatchLineIndexes = [];
    this.fileSearchMatchRangesByLine.clear();
    this.fileSearchActiveIndex = -1;
    this.fileSearchError = null;
  }

  private updateFileSearchResults(lines: string[]): void {
    const query = this.fileSearchQuery.trim();
    if (!query) {
      this.resetSearchResults();
      return;
    }

    const compiled = this.buildSearchRegExp(query);
    if (!compiled) {
      this.fileSearchMatchLineIndexes = [];
      this.fileSearchMatchRangesByLine.clear();
      this.fileSearchActiveIndex = -1;
      return;
    }

    const results: SearchResults = {
      matchLineIndexes: [],
      rangesByLine: new Map<number, SearchRange[]>(),
    };

    for (let i = 0; i < lines.length; i++) {
      const ranges = this.findRangesInLine(lines[i], compiled);
      if (ranges.length === 0) continue;
      results.matchLineIndexes.push(i);
      results.rangesByLine.set(i, ranges);
    }

    this.fileSearchError = null;
    this.fileSearchMatchLineIndexes = results.matchLineIndexes;
    this.fileSearchMatchRangesByLine = results.rangesByLine;
    if (results.matchLineIndexes.length === 0) {
      this.fileSearchActiveIndex = -1;
    } else if (
      this.fileSearchActiveIndex < 0 ||
      this.fileSearchActiveIndex >= results.matchLineIndexes.length
    ) {
      this.fileSearchActiveIndex = 0;
    }
  }

  private buildSearchRegExp(query: string): RegExp | null {
    let pattern = query;
    if (!this.fileSearchRegex) {
      pattern = this.escapeRegExp(pattern);
    }
    if (this.fileSearchWholeWord) {
      pattern = `\\b(?:${pattern})\\b`;
    }

    let flags = "g";
    if (!this.fileSearchCaseSensitive) flags += "i";

    try {
      this.fileSearchError = null;
      return new RegExp(pattern, flags);
    } catch (err) {
      console.error("Failed to compile search regex:", err);
      this.fileSearchError = "Invalid regular expression";
      return null;
    }
  }

  private escapeRegExp(value: string): string {
    return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  }

  private findRangesInLine(line: string, regExp: RegExp): SearchRange[] {
    const ranges: SearchRange[] = [];
    const matcher = new RegExp(regExp.source, regExp.flags);
    let match: RegExpExecArray | null = matcher.exec(line);
    while (match !== null) {
      const start = match.index;
      const end = start + match[0].length;
      if (end > start) {
        ranges.push({ start, end });
      }
      if (match[0].length === 0) {
        matcher.lastIndex += 1;
        if (matcher.lastIndex > line.length) break;
      }
      match = matcher.exec(line);
    }
    return ranges;
  }

  private applyInlineSearchHighlights(
    textEl: HTMLElement,
    ranges: SearchRange[],
  ): void {
    if (ranges.length === 0) return;

    const positions = this.collectTextNodePositions(textEl);
    if (positions.length === 0) return;

    const descending = [...ranges].sort((a, b) => b.start - a.start);
    for (const range of descending) {
      const startPos = this.resolveTextPosition(positions, range.start);
      const endPos = this.resolveTextPosition(positions, range.end);
      if (!startPos || !endPos) continue;

      const domRange = document.createRange();
      domRange.setStart(startPos.node, startPos.offset);
      domRange.setEnd(endPos.node, endPos.offset);

      const highlight = document.createElement("span");
      highlight.className = "diff-panel-search-inline-hit";
      try {
        domRange.surroundContents(highlight);
      } catch (err) {
        console.error(
          "Failed to surround search highlight contents, using fallback:",
          err,
        );
        const frag = domRange.extractContents();
        highlight.appendChild(frag);
        domRange.insertNode(highlight);
      }
    }
  }

  private collectTextNodePositions(
    root: HTMLElement,
  ): Array<{ node: Text; start: number; end: number }> {
    const positions: Array<{ node: Text; start: number; end: number }> = [];
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    let offset = 0;
    let current = walker.nextNode();
    while (current) {
      const textNode = current as Text;
      const len = textNode.nodeValue?.length ?? 0;
      if (len > 0) {
        positions.push({ node: textNode, start: offset, end: offset + len });
        offset += len;
      }
      current = walker.nextNode();
    }
    return positions;
  }

  private resolveTextPosition(
    positions: Array<{ node: Text; start: number; end: number }>,
    absoluteOffset: number,
  ): { node: Text; offset: number } | null {
    if (positions.length === 0) return null;

    for (const pos of positions) {
      if (absoluteOffset < pos.start) continue;
      if (absoluteOffset > pos.end) continue;
      return { node: pos.node, offset: absoluteOffset - pos.start };
    }

    const last = positions[positions.length - 1];
    if (absoluteOffset === last.end) {
      return { node: last.node, offset: last.end - last.start };
    }

    return null;
  }

  private scheduleNativeSelectionSync(): void {
    if (this.nativeSelectionTimerId !== null) {
      window.clearTimeout(this.nativeSelectionTimerId);
    }
    this.nativeSelectionTimerId = window.setTimeout(() => {
      this.nativeSelectionTimerId = null;
      this.syncNativeSelectionHighlight();
    }, 100);
  }

  private syncNativeSelectionHighlight(): void {
    this.removeSelectionActions();

    const context = this.getNativeSelectionContext();
    if (!context) return;

    this.renderSelectionActions(
      context.text,
      context.startLine,
      context.endLine,
      context.filePath,
      context.text.split("\n"),
    );
  }

  private getNativeSelectionContext(): NativeSelectionContext | null {
    const tab = this.getActiveTab();
    if (!tab) return null;

    const view = this.container.querySelector<HTMLElement>(
      ".diff-panel-content-view, .diff-panel-sbs-wrapper",
    );
    if (!view) return null;

    const selection = window.getSelection();
    if (!selection || selection.rangeCount === 0 || selection.isCollapsed)
      return null;

    const ranges: Range[] = [];
    for (let i = 0; i < selection.rangeCount; i++) {
      const range = selection.getRangeAt(i);
      if (!range.intersectsNode(view)) continue;
      ranges.push(range);
    }
    if (ranges.length === 0) return null;

    const lineElements = view.querySelectorAll<HTMLElement>(
      ".diff-panel-file-line, .diff-panel-diff-line",
    );
    const selectedElements: HTMLElement[] = [];
    const selectedLineIndexes: number[] = [];
    for (const lineEl of lineElements) {
      if (!ranges.some((range) => range.intersectsNode(lineEl))) continue;
      const lineIndex = this.getSelectableLineIndex(lineEl);
      if (lineIndex === null) continue;
      selectedElements.push(lineEl);
      selectedLineIndexes.push(lineIndex);
    }
    if (selectedElements.length === 0 || selectedLineIndexes.length === 0)
      return null;

    const textParts: string[] = [];
    for (const range of ranges) {
      const text = this.extractRangeText(range);
      if (text.length > 0) textParts.push(text);
    }
    const text = textParts.join("\n");
    if (text.length === 0) return null;

    return {
      text,
      filePath: tab.filePath,
      startLine: Math.min(...selectedLineIndexes),
      endLine: Math.max(...selectedLineIndexes),
      selectedElements,
    };
  }

  private getSelectableLineIndex(lineEl: HTMLElement): number | null {
    const indexRaw =
      lineEl.getAttribute("data-line-index") ??
      lineEl.getAttribute("data-feedback-line-index");
    if (!indexRaw) return null;
    const lineIndex = Number.parseInt(indexRaw, 10);
    return Number.isNaN(lineIndex) ? null : lineIndex;
  }

  private extractRangeText(range: Range): string {
    const fragment = range.cloneContents();
    const temp = document.createElement("div");
    temp.appendChild(fragment);

    for (const el of temp.querySelectorAll(
      ".diff-panel-linenum, .diff-panel-marker, .file-selection-actions, .feedback-container",
    )) {
      el.remove();
    }

    const lineEls = temp.querySelectorAll(
      ".diff-panel-file-line, .diff-panel-diff-line",
    );
    if (lineEls.length === 0) {
      return temp.textContent ?? "";
    }

    const parts: string[] = [];
    for (const lineEl of lineEls) {
      parts.push(lineEl.textContent ?? "");
    }
    return parts.join("\n");
  }

  private async copyEntireFile(tab: DiffTab): Promise<void> {
    try {
      await navigator.clipboard.writeText(tab.content);
      this.fileCopyStatus = "success";
    } catch (err) {
      console.error("Failed to copy file contents to clipboard:", err);
      this.fileCopyStatus = "error";
    }

    this.renderPreservingScroll();

    if (this.fileCopyResetTimer !== null) {
      window.clearTimeout(this.fileCopyResetTimer);
    }
    this.fileCopyResetTimer = window.setTimeout(() => {
      this.fileCopyStatus = "idle";
      this.fileCopyResetTimer = null;
      this.renderPreservingScroll();
    }, 1200);
  }

  private renderDiffContent(tab: DiffTab): HTMLElement {
    if (!tab.content.trim()) {
      const empty = document.createElement("div");
      empty.className = "diff-panel-empty";
      empty.textContent = "No changes";
      return empty;
    }

    if (this.viewMode === "side-by-side") {
      return this.renderSideBySide(tab);
    }
    return this.renderUnified(tab);
  }

  private renderHunkHeaderLine(
    tab: DiffTab,
    hunkIdx: number,
    text: string,
  ): HTMLElement {
    const lineEl = document.createElement("div");
    lineEl.className = "diff-panel-line diff-panel-hunk-header";
    lineEl.setAttribute("data-hunk-index", String(hunkIdx));
    if (hunkIdx === this.currentHunkIndex) {
      lineEl.classList.add("diff-panel-hunk-active");
    }

    if (!this.canExpandInlineHunkContext(tab)) {
      lineEl.textContent = text;
      return lineEl;
    }

    lineEl.classList.add("diff-panel-hunk-header-expandable");

    const expandBtn = document.createElement("button");
    expandBtn.type = "button";
    expandBtn.className = "diff-panel-hunk-expand-btn";
    const expandedBy = this.getHunkContextExpansion(tab, hunkIdx);
    expandBtn.title =
      expandedBy > 0
        ? `Expand hunk (+10 lines, currently +${expandedBy})`
        : "Expand hunk (+10 lines)";
    expandBtn.setAttribute("aria-label", "Expand hunk context");
    expandBtn.textContent = "+";
    expandBtn.addEventListener("click", (e) => {
      e.preventDefault();
      e.stopPropagation();
      this.expandHunkContext(tab, hunkIdx);
    });
    lineEl.appendChild(expandBtn);

    const label = document.createElement("span");
    label.className = "diff-panel-hunk-header-label";
    label.textContent = text;
    lineEl.appendChild(label);
    return lineEl;
  }

  private renderUnified(tab: DiffTab): HTMLElement {
    const view = document.createElement("div");
    view.className = "diff-panel-content-view";

    const wrapper = document.createElement("div");
    wrapper.className = "diff-panel-line-wrapper";

    const parsed = this.getRenderedDiff(tab);
    const lang = detectLanguage(tab.filePath);

    let hunkIdx = -1;
    let fallbackLineIndex = 0;

    for (const line of parsed.lines) {
      if (line.type === "file-header") {
        continue;
      }

      const lineEl = document.createElement("div");
      lineEl.className = "diff-panel-line";

      if (line.type === "hunk-header") {
        hunkIdx++;
        wrapper.appendChild(this.renderHunkHeaderLine(tab, hunkIdx, line.text));
        continue;
      }

      const oldNum = document.createElement("span");
      oldNum.className = "diff-panel-linenum";
      oldNum.textContent =
        line.oldLineNum !== undefined ? String(line.oldLineNum) : "";

      const newNum = document.createElement("span");
      newNum.className = "diff-panel-linenum";
      newNum.textContent =
        line.newLineNum !== undefined ? String(line.newLineNum) : "";

      const marker = document.createElement("span");
      marker.className = "diff-panel-marker";

      const textSpan = document.createElement("span");
      textSpan.className = "diff-panel-line-text";
      textSpan.innerHTML = lang
        ? highlightLine(line.text, lang)
        : escapeHtml(line.text);

      lineEl.classList.add("diff-panel-diff-line");
      lineEl.setAttribute(
        "data-feedback-line-index",
        String(this.resolveDiffFeedbackLineIndex(line, fallbackLineIndex)),
      );
      fallbackLineIndex += 1;

      if (line.type === "added") {
        lineEl.classList.add("diff-panel-added");
        marker.textContent = "+";
      } else if (line.type === "removed") {
        lineEl.classList.add("diff-panel-removed");
        marker.textContent = "-";
      } else {
        lineEl.classList.add("diff-panel-context");
        marker.textContent = " ";
      }

      lineEl.appendChild(oldNum);
      lineEl.appendChild(newNum);
      lineEl.appendChild(marker);
      lineEl.appendChild(textSpan);
      wrapper.appendChild(lineEl);
    }

    view.appendChild(wrapper);
    return view;
  }

  private renderSideBySide(tab: DiffTab): HTMLElement {
    const wrapper = document.createElement("div");
    wrapper.className = "diff-panel-sbs-wrapper";

    const leftPane = document.createElement("div");
    leftPane.className = "diff-panel-sbs-pane diff-panel-sbs-left";

    const rightPane = document.createElement("div");
    rightPane.className = "diff-panel-sbs-pane diff-panel-sbs-right";

    const leftWrapper = document.createElement("div");
    leftWrapper.className = "diff-panel-line-wrapper";

    const rightWrapper = document.createElement("div");
    rightWrapper.className = "diff-panel-line-wrapper";

    const parsed = this.getRenderedDiff(tab);
    const lang = detectLanguage(tab.filePath);

    let hunkIdx = -1;
    let fallbackLineIndex = 0;

    for (const line of parsed.lines) {
      if (line.type === "file-header") continue;

      if (line.type === "hunk-header") {
        hunkIdx++;
        leftWrapper.appendChild(
          this.renderHunkHeaderLine(tab, hunkIdx, line.text),
        );
        rightWrapper.appendChild(
          this.renderHunkHeaderLine(tab, hunkIdx, line.text),
        );
        continue;
      }

      const feedbackLineIndex = this.resolveDiffFeedbackLineIndex(
        line,
        fallbackLineIndex,
      );
      fallbackLineIndex += 1;

      if (line.type === "removed") {
        leftWrapper.appendChild(
          this.buildSbsLine(line, lang, "removed", "left", feedbackLineIndex),
        );
        rightWrapper.appendChild(this.buildSbsEmptyLine());
        continue;
      }

      if (line.type === "added") {
        leftWrapper.appendChild(this.buildSbsEmptyLine());
        rightWrapper.appendChild(
          this.buildSbsLine(line, lang, "added", "right", feedbackLineIndex),
        );
        continue;
      }

      leftWrapper.appendChild(
        this.buildSbsLine(line, lang, "context", "left", feedbackLineIndex),
      );
      rightWrapper.appendChild(
        this.buildSbsLine(line, lang, "context", "right", feedbackLineIndex),
      );
    }

    leftPane.appendChild(leftWrapper);
    rightPane.appendChild(rightWrapper);

    let syncSource: "left" | "right" | null = null;
    const syncScroll = (
      source: HTMLElement,
      target: HTMLElement,
      sourceId: "left" | "right",
    ) => {
      if (syncSource && syncSource !== sourceId) return;
      syncSource = sourceId;
      if (Math.abs(target.scrollTop - source.scrollTop) > 1) {
        target.scrollTop = source.scrollTop;
      }
      requestAnimationFrame(() => {
        if (syncSource === sourceId) {
          syncSource = null;
        }
      });
    };

    leftPane.addEventListener("scroll", () =>
      syncScroll(leftPane, rightPane, "left"),
    );
    rightPane.addEventListener("scroll", () =>
      syncScroll(rightPane, leftPane, "right"),
    );

    wrapper.appendChild(leftPane);
    wrapper.appendChild(rightPane);
    return wrapper;
  }

  private buildSbsLine(
    line: ParsedDiffLine,
    lang: string | null,
    type: "added" | "removed" | "context",
    side: "left" | "right",
    feedbackLineIndex: number,
  ): HTMLElement {
    const el = document.createElement("div");
    el.className = "diff-panel-line diff-panel-diff-line";
    el.setAttribute("data-feedback-line-index", String(feedbackLineIndex));

    if (type === "added") el.classList.add("diff-panel-added");
    if (type === "removed") el.classList.add("diff-panel-removed");
    if (type === "context") el.classList.add("diff-panel-context");

    const num = document.createElement("span");
    num.className = "diff-panel-linenum";
    if (type === "removed") {
      num.textContent =
        line.oldLineNum !== undefined ? String(line.oldLineNum) : "";
    } else if (type === "added") {
      num.textContent =
        line.newLineNum !== undefined ? String(line.newLineNum) : "";
    } else {
      const contextLineNumber =
        side === "left" ? line.oldLineNum : line.newLineNum;
      num.textContent =
        contextLineNumber !== undefined ? String(contextLineNumber) : "";
    }

    const textSpan = document.createElement("span");
    textSpan.className = "diff-panel-line-text";
    textSpan.innerHTML = lang
      ? highlightLine(line.text, lang)
      : escapeHtml(line.text);

    el.appendChild(num);
    el.appendChild(textSpan);
    return el;
  }

  private resolveDiffFeedbackLineIndex(
    line: ParsedDiffLine,
    fallbackLineIndex: number,
  ): number {
    const oneBasedLineIndex = line.newLineNum ?? line.oldLineNum;
    if (oneBasedLineIndex === undefined) return fallbackLineIndex;
    return Math.max(0, oneBasedLineIndex - 1);
  }

  private buildSbsEmptyLine(): HTMLElement {
    const el = document.createElement("div");
    el.className = "diff-panel-line diff-panel-sbs-empty";

    const num = document.createElement("span");
    num.className = "diff-panel-linenum";

    const textSpan = document.createElement("span");
    textSpan.className = "diff-panel-line-text";

    el.appendChild(num);
    el.appendChild(textSpan);
    return el;
  }

  private createFileLineElement(
    tab: DiffTab,
    lineIndex: number,
    lineText: string,
    lang: string | null,
    matchOrderByLine: Map<number, number>,
    allowDeferredHighlight: boolean,
  ): HTMLElement {
    const lineEl = document.createElement("div");
    lineEl.className = "diff-panel-line diff-panel-file-line";
    lineEl.setAttribute("data-line-index", String(lineIndex));

    const num = document.createElement("span");
    num.className = "diff-panel-linenum";
    num.textContent = String(lineIndex + 1);

    const textSpan = document.createElement("span");
    textSpan.className = "diff-panel-line-text";
    this.renderFileLineText(
      tab,
      lineIndex,
      lineText,
      lang,
      textSpan,
      allowDeferredHighlight,
    );

    const matchIndex = matchOrderByLine.get(lineIndex);
    if (matchIndex !== undefined) {
      lineEl.classList.add("diff-panel-search-hit");
      lineEl.setAttribute("data-search-match-index", String(matchIndex));
      if (matchIndex === this.fileSearchActiveIndex) {
        lineEl.classList.add("diff-panel-search-hit-active");
      }
    }

    if (this.fileGoToLineFlashIndex === lineIndex) {
      lineEl.classList.add("diff-panel-goto-flash");
    }

    num.addEventListener("click", (e) =>
      this.handleLineClick(e, lineIndex, tab.filePath),
    );

    lineEl.appendChild(num);
    lineEl.appendChild(textSpan);
    return lineEl;
  }

  private renderFileLineText(
    tab: DiffTab,
    lineIndex: number,
    lineText: string,
    lang: string | null,
    textSpan: HTMLElement,
    allowDeferredHighlight: boolean,
  ): void {
    const ranges = this.fileSearchMatchRangesByLine.get(lineIndex) ?? [];
    if (!lang) {
      textSpan.innerHTML = escapeHtml(lineText);
      if (ranges.length > 0) this.applyInlineSearchHighlights(textSpan, ranges);
      return;
    }

    const cache = this.getHighlightCache(tab);
    const cached = cache.get(lineIndex);
    if (cached !== undefined) {
      textSpan.innerHTML = cached;
      if (ranges.length > 0) this.applyInlineSearchHighlights(textSpan, ranges);
      return;
    }

    if (allowDeferredHighlight) {
      textSpan.innerHTML = escapeHtml(lineText);
      if (ranges.length > 0) this.applyInlineSearchHighlights(textSpan, ranges);
      return;
    }

    const highlighted = highlightLine(lineText, lang);
    cache.set(lineIndex, highlighted);
    textSpan.innerHTML = highlighted;
    if (ranges.length > 0) this.applyInlineSearchHighlights(textSpan, ranges);
  }

  private createVirtualSpacer(heightPx: number): HTMLElement {
    const spacer = document.createElement("div");
    spacer.className = "diff-panel-virtual-spacer";
    spacer.style.height = `${Math.max(0, heightPx)}px`;
    spacer.setAttribute("aria-hidden", "true");
    return spacer;
  }

  private renderVirtualizedFileContent(
    tab: DiffTab,
    lines: string[],
    lang: string | null,
    matchOrderByLine: Map<number, number>,
  ): HTMLElement {
    const view = document.createElement("div");
    view.className =
      "diff-panel-content-view diff-panel-content-view-virtualized";
    if (this.fileWordWrap) view.classList.add("diff-panel-file-word-wrap");

    const wrapper = document.createElement("div");
    wrapper.className = "diff-panel-line-wrapper";
    view.appendChild(wrapper);

    const lineHeightPx = this.estimateFileLineHeight(view);
    const virtualizer = new Virtualizer<HTMLElement, HTMLElement>({
      count: lines.length,
      getScrollElement: () => view,
      estimateSize: () => lineHeightPx,
      overscan: VIRTUAL_FILE_OVERSCAN_LINES,
      scrollToFn: elementScroll,
      observeElementRect,
      observeElementOffset,
      onChange: () => {
        const active = this.virtualizedFileView;
        if (!active || active.tab.id !== tab.id) return;
        this.updateVirtualizedFileViewport(active, false);
      },
    });

    const state: VirtualizedFileViewState = {
      tab,
      viewEl: view,
      wrapperEl: wrapper,
      lines,
      lang,
      lineHeightPx,
      virtualizer,
      teardownVirtualizer: null,
      renderedStart: -1,
      renderedEnd: -1,
      matchOrderByLine,
      highlightRafId: null,
      highlightQueue: [],
      highlightQueuedSet: new Set<number>(),
    };

    this.virtualizedFileView = state;
    state.teardownVirtualizer = virtualizer._didMount();
    virtualizer._willUpdate();
    this.updateVirtualizedFileViewport(state, true);
    window.requestAnimationFrame(() => {
      if (this.virtualizedFileView !== state) return;
      state.virtualizer._willUpdate();
      this.updateVirtualizedFileViewport(state, true);
    });

    view.addEventListener("click", (e) => {
      const target = e.target as Node;
      if (target === view || target === wrapper) {
        this.clearSelection();
        this.scheduleNativeSelectionSync();
      }
    });

    return view;
  }

  private updateVirtualizedFileViewport(
    state: VirtualizedFileViewState,
    force: boolean,
  ): void {
    if (this.virtualizedFileView !== state) return;
    state.virtualizer._willUpdate();

    const totalLines = state.lines.length;
    const virtualItems = state.virtualizer.getVirtualItems();
    if (virtualItems.length === 0) {
      const totalSize = Math.max(
        state.virtualizer.getTotalSize(),
        totalLines * state.lineHeightPx,
      );
      state.wrapperEl.replaceChildren(this.createVirtualSpacer(totalSize));
      state.renderedStart = 0;
      state.renderedEnd = 0;
      return;
    }

    const first = virtualItems[0];
    const last = virtualItems[virtualItems.length - 1];
    const start = first.index;
    const end = last.index + 1;
    if (!force && start === state.renderedStart && end === state.renderedEnd)
      return;

    state.renderedStart = start;
    state.renderedEnd = end;

    const frag = document.createDocumentFragment();
    const totalSize = Math.max(
      state.virtualizer.getTotalSize(),
      totalLines * state.lineHeightPx,
    );
    const topSpacerSize = Math.max(0, first.start);
    if (topSpacerSize > 0) {
      frag.appendChild(this.createVirtualSpacer(topSpacerSize));
    }

    for (const item of virtualItems) {
      const i = item.index;
      const lineEl = this.createFileLineElement(
        state.tab,
        i,
        state.lines[i] ?? "",
        state.lang,
        state.matchOrderByLine,
        true,
      );
      frag.appendChild(lineEl);

      for (const box of this.feedbackBoxes.values()) {
        if (box.filePath !== state.tab.filePath) continue;
        if (box.endLine === i) frag.appendChild(box.element);
      }
    }

    const bottomSpacerSize = Math.max(0, totalSize - last.end);
    if (bottomSpacerSize > 0) {
      frag.appendChild(this.createVirtualSpacer(bottomSpacerSize));
    } else {
      for (const box of this.feedbackBoxes.values()) {
        if (box.filePath !== state.tab.filePath) continue;
        if (box.endLine >= totalLines) frag.appendChild(box.element);
      }
    }

    state.wrapperEl.replaceChildren(frag);
    this.enqueueVirtualizedHighlights(
      state,
      virtualItems.map((item) => item.index),
    );
  }

  private enqueueVirtualizedHighlights(
    state: VirtualizedFileViewState,
    visibleLineIndexes: number[],
  ): void {
    if (!state.lang) return;
    const cache = this.getHighlightCache(state.tab);
    for (const i of visibleLineIndexes) {
      if (cache.has(i) || state.highlightQueuedSet.has(i)) continue;
      state.highlightQueue.push(i);
      state.highlightQueuedSet.add(i);
    }

    if (state.highlightRafId !== null) return;

    const process = () => {
      if (this.virtualizedFileView !== state) return;

      const activeCache = this.getHighlightCache(state.tab);
      let processed = 0;
      while (
        state.highlightQueue.length > 0 &&
        processed < VIRTUAL_HIGHLIGHT_BATCH_SIZE
      ) {
        const lineIndex = state.highlightQueue.shift();
        if (lineIndex === undefined) break;
        state.highlightQueuedSet.delete(lineIndex);
        if (activeCache.has(lineIndex)) continue;

        const lineText = state.lines[lineIndex] ?? "";
        const highlighted = highlightLine(lineText, state.lang!);
        activeCache.set(lineIndex, highlighted);

        if (lineIndex >= state.renderedStart && lineIndex < state.renderedEnd) {
          const textSpan = state.wrapperEl.querySelector<HTMLElement>(
            `.diff-panel-file-line[data-line-index="${lineIndex}"] .diff-panel-line-text`,
          );
          if (textSpan) {
            textSpan.innerHTML = highlighted;
            const ranges =
              this.fileSearchMatchRangesByLine.get(lineIndex) ?? [];
            if (ranges.length > 0)
              this.applyInlineSearchHighlights(textSpan, ranges);
          }
        }
        processed += 1;
      }

      if (state.highlightQueue.length > 0) {
        state.highlightRafId = window.requestAnimationFrame(process);
      } else {
        state.highlightRafId = null;
      }
    };

    state.highlightRafId = window.requestAnimationFrame(process);
  }

  private estimateFileLineHeight(view: HTMLElement): number {
    const computed = window.getComputedStyle(view);
    const fontSizePx = Number.parseFloat(computed.fontSize) || 13;
    const rawLineHeight = computed.lineHeight;
    let lineHeightPx = Number.parseFloat(rawLineHeight);

    if (!Number.isFinite(lineHeightPx) || rawLineHeight === "normal") {
      const rootStyles = window.getComputedStyle(document.documentElement);
      const lineHeightFactor = Number.parseFloat(
        rootStyles.getPropertyValue("--editor-line-height"),
      );
      if (Number.isFinite(lineHeightFactor) && lineHeightFactor > 0) {
        lineHeightPx = fontSizePx * lineHeightFactor;
      }
    }

    if (!Number.isFinite(lineHeightPx) || lineHeightPx <= 0) {
      lineHeightPx = fontSizePx * 1.5;
    }
    return Math.max(12, lineHeightPx);
  }

  private renderFileContent(tab: DiffTab): HTMLElement {
    const view = document.createElement("div");
    view.className = "diff-panel-content-view";
    if (this.fileWordWrap) view.classList.add("diff-panel-file-word-wrap");

    const lang = detectLanguage(tab.filePath);
    const lines = this.getFileLines(tab);
    this.updateFileSearchResults(lines);
    const matchOrderByLine = new Map<number, number>();
    this.fileSearchMatchLineIndexes.forEach((lineIdx, order) => {
      matchOrderByLine.set(lineIdx, order);
    });

    if (this.shouldVirtualizeFileContent()) {
      return this.renderVirtualizedFileContent(
        tab,
        lines,
        lang,
        matchOrderByLine,
      );
    }

    const wrapper = document.createElement("div");
    wrapper.className = "diff-panel-line-wrapper";

    for (let i = 0; i < lines.length; i++) {
      const lineEl = this.createFileLineElement(
        tab,
        i,
        lines[i],
        lang,
        matchOrderByLine,
        false,
      );
      wrapper.appendChild(lineEl);

      for (const box of this.feedbackBoxes.values()) {
        if (box.filePath !== tab.filePath) continue;
        if (box.endLine === i) wrapper.appendChild(box.element);
      }
    }

    for (const box of this.feedbackBoxes.values()) {
      if (box.filePath !== tab.filePath) continue;
      if (box.endLine >= lines.length) wrapper.appendChild(box.element);
    }

    view.appendChild(wrapper);

    view.addEventListener("click", (e) => {
      const target = e.target as Node;
      if (target === view || target === wrapper) {
        this.clearSelection();
        this.scheduleNativeSelectionSync();
      }
    });

    return view;
  }

  private navigateHunk(direction: number): void {
    const tab = this.getActiveTab();
    if (!tab || tab.type !== "diff") return;

    const parsed = this.getRenderedDiff(tab);
    const hunkCount = parsed.hunks.length;
    if (hunkCount === 0) return;

    let next = this.currentHunkIndex + direction;
    if (next < 0) next = hunkCount - 1;
    if (next >= hunkCount) next = 0;

    this.currentHunkIndex = next;
    this.render();

    const hunkEl = this.container.querySelector(`[data-hunk-index="${next}"]`);
    if (hunkEl) hunkEl.scrollIntoView({ behavior: "smooth", block: "center" });
  }

  private handleLineClick(
    e: MouseEvent,
    lineIndex: number,
    _filePath: string,
  ): void {
    e.stopPropagation();

    if (this.selectionAnchor !== null && e.shiftKey) {
      this.selectionEnd = lineIndex;
      this.selectionComplete = true;
      this.updateLineSelection();
      return;
    }

    if (this.selectionComplete) {
      this.clearSelectionVisuals();
      this.selectionAnchor = lineIndex;
      this.selectionEnd = null;
      this.selectionComplete = false;
      this.updateLineSelection();
      return;
    }

    if (this.selectionAnchor !== null && this.selectionAnchor === lineIndex) {
      this.clearSelection();
      return;
    }

    if (this.selectionAnchor !== null) {
      this.selectionEnd = lineIndex;
      this.selectionComplete = true;
      this.updateLineSelection();
      return;
    }

    this.selectionAnchor = lineIndex;
    this.selectionEnd = null;
    this.selectionComplete = false;
    this.updateLineSelection();
  }

  private updateLineSelection(): void {
    this.removeSelectionActions();
    if (this.selectionAnchor === null) return;
    if (!this.selectionComplete || this.selectionEnd === null) return;

    const tab = this.getActiveTab();
    if (!tab || tab.type !== "file") return;

    const start = Math.min(this.selectionAnchor, this.selectionEnd);
    const end = Math.max(this.selectionAnchor, this.selectionEnd);
    const lines = this.getFileLines(tab).slice(start, end + 1);

    this.renderSelectionActions(
      lines.join("\n"),
      start,
      end,
      tab.filePath,
      lines,
    );
  }

  private removeSelectionActions(): void {
    for (const el of this.container.querySelectorAll(
      ".file-selection-actions",
    )) {
      el.remove();
    }
  }

  private renderSelectionActions(
    text: string,
    startLine: number,
    endLine: number,
    filePath: string,
    lines: string[],
  ): void {
    this.removeSelectionActions();

    const view = this.container.querySelector<HTMLElement>(
      ".diff-panel-content-view, .diff-panel-sbs-wrapper",
    );
    if (!view) return;

    const actions = document.createElement("div");
    actions.className = "file-selection-actions";

    const label = document.createElement("span");
    label.className = "file-selection-action-label";
    label.textContent =
      startLine === endLine
        ? `Line ${startLine + 1}`
        : `Lines ${startLine + 1}\u2013${endLine + 1}`;
    actions.appendChild(label);

    const copyBtn = document.createElement("button");
    copyBtn.type = "button";
    copyBtn.className = "file-selection-action-btn";
    copyBtn.textContent = "Copy";
    copyBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      void this.copyTextWithStatus(text, copyBtn);
    });
    actions.appendChild(copyBtn);

    const feedbackBtn = document.createElement("button");
    feedbackBtn.type = "button";
    feedbackBtn.className =
      "file-selection-action-btn file-selection-action-primary";
    feedbackBtn.textContent = "Give Feedback";
    feedbackBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      this.showFeedbackInput(startLine, endLine, filePath, lines);
    });
    actions.appendChild(feedbackBtn);

    view.appendChild(actions);
  }

  private async copyTextWithStatus(
    text: string,
    button: HTMLButtonElement,
  ): Promise<void> {
    const originalLabel = button.textContent ?? "Copy";
    try {
      await navigator.clipboard.writeText(text);
      button.textContent = "Copied";
    } catch (err) {
      console.error("Failed to copy text to clipboard:", err);
      button.textContent = "Failed";
    }
    window.setTimeout(() => {
      button.textContent = originalLabel;
    }, 1200);
  }

  clearSelection(): void {
    this.selectionAnchor = null;
    this.selectionEnd = null;
    this.selectionComplete = false;
    this.clearSelectionVisuals();
  }

  private clearSelectionVisuals(): void {
    this.removeSelectionActions();
  }

  private handleFileSelectionCopy(e: ClipboardEvent): void {
    if (!e.clipboardData) return;
    const context = this.getNativeSelectionContext();
    if (!context) return;
    e.preventDefault();
    e.clipboardData.setData("text/plain", context.text);
  }

  private resolveFeedbackLayoutHost(
    anchorElement: Element | undefined,
    view: HTMLElement,
  ): HTMLElement {
    const anchorHost =
      anchorElement instanceof HTMLElement
        ? anchorElement.closest<HTMLElement>(".diff-panel-sbs-pane")
        : null;
    if (anchorHost) return anchorHost;

    if (view.classList.contains("diff-panel-sbs-pane")) {
      return view;
    }

    const fallbackPane = view.querySelector<HTMLElement>(
      ".diff-panel-sbs-pane",
    );
    if (fallbackPane) return fallbackPane;

    return view;
  }

  private applyFeedbackContainerLayout(
    feedbackContainer: HTMLElement,
    host: HTMLElement,
  ): void {
    const hostWidth = host.clientWidth;
    if (hostWidth <= 0) return;
    const width = Math.min(680, Math.max(260, hostWidth - 64));
    feedbackContainer.style.width = `${width}px`;
    feedbackContainer.style.maxWidth = `${width}px`;
  }

  showFeedbackInput(
    startLine: number,
    endLine: number,
    filePath: string,
    lineContents: string[],
    anchorElement?: Element,
  ): void {
    const key = `${filePath}:${startLine}-${endLine}`;
    for (const actionEl of this.container.querySelectorAll(
      ".file-selection-actions",
    )) {
      actionEl.remove();
    }

    const feedbackContainer = document.createElement("div");
    feedbackContainer.className = "feedback-container";

    const header = document.createElement("div");
    header.className = "feedback-header";
    header.textContent = `Feedback on lines ${startLine + 1}-${endLine + 1}`;

    const textarea = document.createElement("textarea");
    textarea.className = "feedback-textarea";
    textarea.placeholder = "Describe the changes you'd like...";

    const actions = document.createElement("div");
    actions.className = "feedback-actions";

    const cancelBtn = document.createElement("button");
    cancelBtn.className = "feedback-cancel-btn";
    cancelBtn.textContent = "Cancel";
    cancelBtn.addEventListener("click", () => {
      feedbackContainer.remove();
      this.feedbackBoxes.delete(key);
      this.clearSelection();
    });

    const submitBtn = document.createElement("button");
    submitBtn.className = "feedback-submit-btn";
    submitBtn.textContent = "Submit";
    submitBtn.addEventListener("click", () => {
      this.submitFeedback(key, textarea.value, lineContents.join("\n"));
    });

    actions.appendChild(cancelBtn);
    actions.appendChild(submitBtn);
    feedbackContainer.appendChild(header);
    feedbackContainer.appendChild(textarea);
    feedbackContainer.appendChild(actions);

    const view = this.container.querySelector<HTMLElement>(
      ".diff-panel-content-view, .diff-panel-sbs-wrapper",
    );
    if (!view) return;

    let layoutAnchor: Element | undefined = anchorElement;
    if (anchorElement?.isConnected) {
      anchorElement.after(feedbackContainer);
    } else {
      const lineEls = view.querySelectorAll<HTMLElement>(
        ".diff-panel-file-line, .diff-panel-diff-line",
      );
      const fallbackAnchor = Array.from(lineEls)
        .reverse()
        .find((lineEl) => this.getSelectableLineIndex(lineEl) === endLine);
      if (fallbackAnchor) {
        fallbackAnchor.after(feedbackContainer);
        layoutAnchor = fallbackAnchor;
      } else {
        view.appendChild(feedbackContainer);
        layoutAnchor = view;
      }
    }

    const feedbackHost = this.resolveFeedbackLayoutHost(layoutAnchor, view);
    this.applyFeedbackContainerLayout(feedbackContainer, feedbackHost);

    const box: FeedbackBox = {
      startLine,
      endLine,
      filePath,
      conversationId: null,
      element: feedbackContainer,
      status: "input",
      summary: "",
    };
    this.feedbackBoxes.set(key, box);

    textarea.focus();
  }

  private async submitFeedback(
    key: string,
    feedback: string,
    lineContent: string,
  ): Promise<void> {
    const box = this.feedbackBoxes.get(key);
    if (!box || !this.onFeedbackSubmit) return;

    box.status = "progress";
    box.summary = "Starting...";
    this.renderFeedbackProgress(box, feedback);

    try {
      const convId = await this.onFeedbackSubmit(
        box.filePath,
        box.startLine,
        box.endLine,
        lineContent,
        feedback,
      );
      box.conversationId = convId;
    } catch (err) {
      console.error("Failed to submit feedback:", err);
      box.status = "error";
      box.summary = "Submission failed";
      const summaryEl = box.element.querySelector(".feedback-summary");
      if (summaryEl) summaryEl.textContent = box.summary;
      const spinnerEl = box.element.querySelector(".feedback-spinner");
      if (!spinnerEl) return;
      spinnerEl.className = "feedback-error-icon";
      spinnerEl.textContent = "✗";
    }
  }

  private renderFeedbackProgress(box: FeedbackBox, feedbackText: string): void {
    const el = box.element;
    el.innerHTML = "";

    const header = document.createElement("div");
    header.className = "feedback-header";
    header.textContent = `Feedback on lines ${box.startLine + 1}-${box.endLine + 1}`;

    const progress = document.createElement("div");
    progress.className = "feedback-progress";

    const spinner = document.createElement("span");
    spinner.className = "feedback-spinner";

    const summary = document.createElement("span");
    summary.className = "feedback-summary";
    summary.textContent = box.summary;

    progress.appendChild(spinner);
    progress.appendChild(summary);

    const original = document.createElement("div");
    original.className = "feedback-original";
    original.textContent = feedbackText;

    el.appendChild(header);
    el.appendChild(progress);
    el.appendChild(original);
  }

  updateFeedbackProgress(
    conversationId: number,
    summary: string,
    status: "progress" | "complete" | "error",
  ): void {
    for (const box of this.feedbackBoxes.values()) {
      if (box.conversationId !== conversationId) continue;

      box.summary = summary;
      box.status = status;

      const summaryEl = box.element.querySelector(".feedback-summary");
      if (summaryEl) summaryEl.textContent = summary;

      const spinnerEl = box.element.querySelector(".feedback-spinner");
      if (!spinnerEl) break;

      if (status === "complete") {
        spinnerEl.className = "feedback-complete-icon";
        spinnerEl.textContent = "✓";
      } else if (status === "error") {
        spinnerEl.className = "feedback-error-icon";
        spinnerEl.textContent = "✗";
      }
      break;
    }
  }

  refreshFileContent(filePath: string, newContent: string): void {
    const tab = this.tabs.find(
      (t) => t.filePath === filePath && t.type === "file",
    );
    if (!tab) return;
    if (tab.content === newContent) return;

    tab.content = newContent;
    this.invalidateFileCaches(tab);
    if (this.activeTabId !== tab.id) return;
    this.render();
  }
}
