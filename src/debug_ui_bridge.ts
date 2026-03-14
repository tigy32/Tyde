import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { domToCanvas } from "modern-screenshot";
import { submitDebugUiResponse } from "./bridge";

interface DebugUiRequestPayload {
  request_id: string;
  action: string;
  params?: unknown;
}

const DEFAULT_WAIT_TIMEOUT_MS = 5_000;
const MAX_WAIT_TIMEOUT_MS = 60_000;
const DEFAULT_MAX_SCREENSHOT_DIMENSION = 2_048;
const MAX_MAX_SCREENSHOT_DIMENSION = 8_192;
const DEFAULT_MAX_QUERY_NODES = 100;
const MAX_MAX_QUERY_NODES = 1_000;
const DEFAULT_MAX_TEXT_LENGTH = 20_000;
const MAX_MAX_TEXT_LENGTH = 100_000;
const OVERFLOW_CLIPPING_VALUES = new Set(["auto", "scroll", "hidden", "clip"]);

function asObject(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) return {};
  return value as Record<string, unknown>;
}

function asString(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

function asBoolean(value: unknown, fallback: boolean): boolean {
  return typeof value === "boolean" ? value : fallback;
}

function asNumber(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function asInteger(value: unknown): number | null {
  const parsed = asNumber(value);
  if (parsed === null) return null;
  return Math.trunc(parsed);
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function normalizeTimeoutMs(value: unknown): number {
  const parsed = asInteger(value);
  if (parsed === null) return DEFAULT_WAIT_TIMEOUT_MS;
  return clamp(parsed, 1, MAX_WAIT_TIMEOUT_MS);
}

function normalizeMaxNodes(value: unknown): number {
  const parsed = asInteger(value);
  if (parsed === null) return DEFAULT_MAX_QUERY_NODES;
  return clamp(parsed, 1, MAX_MAX_QUERY_NODES);
}

function normalizeMaxTextLength(value: unknown): number {
  const parsed = asInteger(value);
  if (parsed === null) return DEFAULT_MAX_TEXT_LENGTH;
  return clamp(parsed, 1, MAX_MAX_TEXT_LENGTH);
}

function normalizeMaxScreenshotDimension(value: unknown): number {
  const parsed = asInteger(value);
  if (parsed === null) return DEFAULT_MAX_SCREENSHOT_DIMENSION;
  return clamp(parsed, 64, MAX_MAX_SCREENSHOT_DIMENSION);
}

function normalizeIndex(value: unknown): number {
  const parsed = asInteger(value);
  if (parsed === null) return 0;
  return Math.max(0, parsed);
}

function truncate(text: string, maxLength: number): string {
  if (text.length <= maxLength) return text;
  return `${text.slice(0, maxLength)}…`;
}

function getElementAt(selector: string, indexRaw: unknown): Element {
  const index = normalizeIndex(indexRaw);
  const all = Array.from(document.querySelectorAll(selector));
  if (all.length === 0) {
    throw new Error(`No elements found for selector: ${selector}`);
  }
  if (index >= all.length) {
    throw new Error(
      `Selector ${selector} has ${all.length} matches; index ${index} is out of range`,
    );
  }
  return all[index];
}

function isVisible(el: Element): boolean {
  const rect = el.getBoundingClientRect();
  if (rect.width <= 0 || rect.height <= 0) return false;
  const style = window.getComputedStyle(el);
  return (
    style.display !== "none" &&
    style.visibility !== "hidden" &&
    style.opacity !== "0"
  );
}

function elementRect(el: Element): Record<string, number> {
  const rect = el.getBoundingClientRect();
  return {
    x: Math.round(rect.x),
    y: Math.round(rect.y),
    width: Math.round(rect.width),
    height: Math.round(rect.height),
    top: Math.round(rect.top),
    right: Math.round(rect.right),
    bottom: Math.round(rect.bottom),
    left: Math.round(rect.left),
  };
}

type RectBounds = {
  left: number;
  top: number;
  right: number;
  bottom: number;
  width: number;
  height: number;
};

function rectBounds(rect: DOMRect | DOMRectReadOnly): RectBounds {
  return {
    left: rect.left,
    top: rect.top,
    right: rect.right,
    bottom: rect.bottom,
    width: rect.width,
    height: rect.height,
  };
}

function intersectRectBounds(a: RectBounds, b: RectBounds): RectBounds {
  const left = Math.max(a.left, b.left);
  const top = Math.max(a.top, b.top);
  const right = Math.min(a.right, b.right);
  const bottom = Math.min(a.bottom, b.bottom);
  return {
    left,
    top,
    right,
    bottom,
    width: Math.max(0, right - left),
    height: Math.max(0, bottom - top),
  };
}

function rectsIntersect(a: RectBounds, b: RectBounds): boolean {
  if (a.width <= 0 || a.height <= 0) return false;
  if (b.width <= 0 || b.height <= 0) return false;
  return (
    a.left < b.right && a.right > b.left && a.top < b.bottom && a.bottom > b.top
  );
}

function elementClipsOverflow(el: Element): boolean {
  const style = window.getComputedStyle(el);
  return (
    OVERFLOW_CLIPPING_VALUES.has(style.overflowX) ||
    OVERFLOW_CLIPPING_VALUES.has(style.overflowY)
  );
}

function buildVisibleScreenshotFilter(
  captureRoot: Element,
  captureRect: DOMRect | DOMRectReadOnly,
): (node: Node) => boolean {
  const captureBounds = rectBounds(captureRect);
  const visibilityCache = new WeakMap<Element, boolean>();
  const clipBoundsCache = new WeakMap<Element, RectBounds>();

  function clipBoundsFor(el: Element): RectBounds {
    const cached = clipBoundsCache.get(el);
    if (cached) return cached;

    let bounds = captureBounds;
    for (
      let current = el.parentElement;
      current;
      current = current.parentElement
    ) {
      if (current === captureRoot) break;
      if (!elementClipsOverflow(current)) continue;
      bounds = intersectRectBounds(
        bounds,
        rectBounds(current.getBoundingClientRect()),
      );
      if (bounds.width <= 0 || bounds.height <= 0) break;
    }

    clipBoundsCache.set(el, bounds);
    return bounds;
  }

  return (node: Node): boolean => {
    if (!(node instanceof Element)) return true;
    if (node === captureRoot) return true;

    const cached = visibilityCache.get(node);
    if (cached !== undefined) return cached;

    const nodeRect = node.getBoundingClientRect();
    // Keep zero-sized structural nodes; some are purely positional wrappers for visible children.
    if (nodeRect.width <= 0 || nodeRect.height <= 0) {
      visibilityCache.set(node, true);
      return true;
    }

    const visible = rectsIntersect(rectBounds(nodeRect), clipBoundsFor(node));
    visibilityCache.set(node, visible);
    return visible;
  };
}

async function handleQueryElements(
  params: Record<string, unknown>,
): Promise<unknown> {
  const selector = asString(params.selector);
  if (!selector || selector.trim().length === 0) {
    throw new Error("query_elements requires non-empty selector");
  }
  const maxNodes = normalizeMaxNodes(params.max_nodes);
  const includeText = asBoolean(params.include_text, true);
  const includeHtml = asBoolean(params.include_html, false);
  const maxText = normalizeMaxTextLength(params.max_text_length);

  const nodes = Array.from(document.querySelectorAll(selector)).slice(
    0,
    maxNodes,
  );
  const elements = nodes.map((node, index) => {
    const htmlEl = node as HTMLElement;
    const info: Record<string, unknown> = {
      index,
      tag: node.tagName.toLowerCase(),
      id: htmlEl.id || null,
      class_name: htmlEl.className || null,
      data_testid: htmlEl.getAttribute("data-testid"),
      visible: isVisible(node),
      disabled:
        (htmlEl as HTMLButtonElement | HTMLInputElement).disabled ?? false,
      rect: elementRect(node),
    };
    if (includeText) {
      info.text = truncate(
        (htmlEl.innerText || htmlEl.textContent || "").trim(),
        maxText,
      );
    }
    if (includeHtml) {
      info.html = truncate(htmlEl.outerHTML, maxText);
    }
    return info;
  });

  return {
    selector,
    count: document.querySelectorAll(selector).length,
    returned: elements.length,
    elements,
  };
}

async function handleGetText(
  params: Record<string, unknown>,
): Promise<unknown> {
  const selector = asString(params.selector);
  if (!selector || selector.trim().length === 0) {
    throw new Error("get_text requires non-empty selector");
  }
  const maxLength = normalizeMaxTextLength(params.max_length);
  const el = getElementAt(selector, params.index);
  let text = "";

  if (el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement) {
    text = el.value;
  } else if (el instanceof HTMLSelectElement) {
    text = el.value;
  } else {
    text = (el as HTMLElement).innerText || el.textContent || "";
  }

  return {
    selector,
    index: normalizeIndex(params.index),
    text: truncate(text, maxLength),
    length: text.length,
  };
}

async function handleListTestIds(
  params: Record<string, unknown>,
): Promise<unknown> {
  const patternRaw = asString(params.pattern);
  const pattern = patternRaw ? patternRaw.toLowerCase() : null;
  const counts = new Map<string, number>();

  for (const el of Array.from(document.querySelectorAll("[data-testid]"))) {
    const testId = el.getAttribute("data-testid");
    if (!testId) continue;
    if (pattern && !testId.toLowerCase().includes(pattern)) continue;
    counts.set(testId, (counts.get(testId) ?? 0) + 1);
  }

  return {
    total_unique: counts.size,
    ids: Array.from(counts.entries())
      .sort((a, b) => a[0].localeCompare(b[0]))
      .map(([testid, count]) => ({ testid, count })),
  };
}

async function handleClick(params: Record<string, unknown>): Promise<unknown> {
  const selector = asString(params.selector);
  if (!selector || selector.trim().length === 0) {
    throw new Error("click requires non-empty selector");
  }
  const el = getElementAt(selector, params.index);
  const tag = el.tagName.toLowerCase();
  const visible = isVisible(el);
  if (el instanceof HTMLElement) {
    el.focus();
  }
  el.dispatchEvent(
    new MouseEvent("mousedown", { bubbles: true, cancelable: true }),
  );
  if (el instanceof HTMLElement) {
    el.click();
  } else {
    el.dispatchEvent(
      new MouseEvent("click", { bubbles: true, cancelable: true }),
    );
  }
  el.dispatchEvent(
    new MouseEvent("mouseup", { bubbles: true, cancelable: true }),
  );

  return {
    selector,
    index: normalizeIndex(params.index),
    clicked: true,
    tag,
    visible,
  };
}

async function handleType(params: Record<string, unknown>): Promise<unknown> {
  const selector = asString(params.selector);
  const text = asString(params.text);
  if (!selector || selector.trim().length === 0) {
    throw new Error("type requires non-empty selector");
  }
  if (text === null) {
    throw new Error("type requires text");
  }

  const append = asBoolean(params.append, false);
  const submit = asBoolean(params.submit, false);
  const el = getElementAt(selector, params.index);

  if (el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement) {
    const next = append ? `${el.value}${text}` : text;
    el.focus();
    el.value = next;
    el.dispatchEvent(new Event("input", { bubbles: true }));
    el.dispatchEvent(new Event("change", { bubbles: true }));
    if (submit) {
      el.dispatchEvent(
        new KeyboardEvent("keydown", { key: "Enter", bubbles: true }),
      );
      el.dispatchEvent(
        new KeyboardEvent("keyup", { key: "Enter", bubbles: true }),
      );
    }
    return {
      selector,
      index: normalizeIndex(params.index),
      value_length: next.length,
      submitted: submit,
    };
  }

  if (el instanceof HTMLElement && el.isContentEditable) {
    const current = el.textContent ?? "";
    const next = append ? `${current}${text}` : text;
    el.focus();
    el.textContent = next;
    el.dispatchEvent(new Event("input", { bubbles: true }));
    if (submit) {
      el.dispatchEvent(
        new KeyboardEvent("keydown", { key: "Enter", bubbles: true }),
      );
      el.dispatchEvent(
        new KeyboardEvent("keyup", { key: "Enter", bubbles: true }),
      );
    }
    return {
      selector,
      index: normalizeIndex(params.index),
      value_length: next.length,
      submitted: submit,
    };
  }

  throw new Error(`Element matched by ${selector} is not typable`);
}

async function handleKeypress(
  params: Record<string, unknown>,
): Promise<unknown> {
  const key = asString(params.key);
  if (!key || key.trim().length === 0) {
    throw new Error("keypress requires non-empty key");
  }

  const eventInit: KeyboardEventInit = {
    key,
    code: asString(params.code) ?? undefined,
    ctrlKey: asBoolean(params.ctrl, false),
    altKey: asBoolean(params.alt, false),
    shiftKey: asBoolean(params.shift, false),
    metaKey: asBoolean(params.meta, false),
    bubbles: true,
    cancelable: true,
  };
  const target = (document.activeElement ?? document.body) as HTMLElement;
  target.dispatchEvent(new KeyboardEvent("keydown", eventInit));
  target.dispatchEvent(new KeyboardEvent("keyup", eventInit));
  return {
    key,
    target_tag: target.tagName.toLowerCase(),
  };
}

async function handleScroll(params: Record<string, unknown>): Promise<unknown> {
  const dx = asNumber(params.dx) ?? 0;
  const dy = asNumber(params.dy) ?? 0;
  const selector = asString(params.selector);

  if (!selector || selector.trim().length === 0) {
    window.scrollBy(dx, dy);
    return {
      scoped_to: "window",
      x: window.scrollX,
      y: window.scrollY,
    };
  }

  const el = getElementAt(selector, params.index);
  if (!(el instanceof HTMLElement)) {
    throw new Error("scroll target is not an HTMLElement");
  }
  el.scrollBy(dx, dy);
  return {
    scoped_to: "element",
    selector,
    index: normalizeIndex(params.index),
    scroll_left: el.scrollLeft,
    scroll_top: el.scrollTop,
  };
}

function waitConditionMet(
  state: string,
  selector: string,
  indexRaw: unknown,
): boolean {
  const allMatches = Array.from(document.querySelectorAll(selector));
  const index = asInteger(indexRaw);
  const scopedMatches =
    index === null
      ? allMatches
      : index >= 0 && index < allMatches.length
        ? [allMatches[index]]
        : [];
  const hasVisible = scopedMatches.some((node) => isVisible(node));
  const allHidden = scopedMatches.every((node) => !isVisible(node));
  switch (state) {
    case "exists":
      return scopedMatches.length > 0;
    case "visible":
      return hasVisible;
    case "hidden":
      return scopedMatches.length > 0 && allHidden;
    case "gone":
      return index === null
        ? allMatches.length === 0
        : index >= allMatches.length;
    default:
      return false;
  }
}

async function handleWaitFor(
  params: Record<string, unknown>,
): Promise<unknown> {
  const selector = asString(params.selector);
  if (!selector || selector.trim().length === 0) {
    throw new Error("wait_for requires non-empty selector");
  }
  const stateRaw = asString(params.state) ?? "visible";
  const state = stateRaw.trim().toLowerCase();
  if (!["exists", "visible", "hidden", "gone"].includes(state)) {
    throw new Error(`Unsupported wait_for state '${stateRaw}'`);
  }

  const timeoutMs = normalizeTimeoutMs(params.timeout_ms);
  const index = asInteger(params.index);
  const started = Date.now();
  while (Date.now() - started <= timeoutMs) {
    if (waitConditionMet(state, selector, params.index)) {
      return {
        selector,
        index,
        state,
        waited_ms: Date.now() - started,
      };
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(
    `Timed out waiting for selector '${selector}' in state '${state}'`,
  );
}

async function handleCaptureScreenshot(
  params: Record<string, unknown>,
): Promise<unknown> {
  const selector = asString(params.selector);
  const maxDimension = normalizeMaxScreenshotDimension(params.max_dimension);

  let node: Element;
  let captureRect: DOMRect;
  let sourceWidth: number;
  let sourceHeight: number;
  let selectorOut: string | null;
  let indexOut: number | null;

  if (selector && selector.trim().length > 0) {
    node = getElementAt(selector, params.index);
    captureRect = node.getBoundingClientRect();
    if (captureRect.width <= 0 || captureRect.height <= 0) {
      throw new Error(
        `Cannot capture screenshot for selector '${selector}': target is hidden or has zero size`,
      );
    }
    sourceWidth = captureRect.width;
    sourceHeight = captureRect.height;
    selectorOut = selector;
    indexOut = normalizeIndex(params.index);
  } else {
    // Use the #app root container, not document.body — modern-screenshot clones
    // the node into an SVG <foreignObject>, and <body> is not valid XHTML inside
    // <foreignObject>, causing WebKit to render it blank.
    const appRoot = document.getElementById("app");
    if (!appRoot) {
      throw new Error(
        "Viewport screenshot failed: #app root element not found",
      );
    }
    node = appRoot;
    captureRect = node.getBoundingClientRect();
    sourceWidth = Math.max(1, window.innerWidth);
    sourceHeight = Math.max(1, window.innerHeight);
    selectorOut = null;
    indexOut = null;
  }

  const sourceMax = Math.max(sourceWidth, sourceHeight);
  const scale = sourceMax > maxDimension ? maxDimension / sourceMax : 1;

  await document.fonts.ready;

  const canvas = await domToCanvas(node, {
    debug: true,
    width: sourceWidth,
    height: sourceHeight,
    scale,
    backgroundColor:
      selectorOut === null
        ? getComputedStyle(document.body).backgroundColor || "#fff"
        : undefined,
    filter: buildVisibleScreenshotFilter(node, captureRect),
    style:
      selectorOut === null ? { overflow: "hidden", margin: "0" } : undefined,
  });

  const outputWidth = canvas.width;
  const outputHeight = canvas.height;

  const dataUrl = canvas.toDataURL("image/png");
  const pngPrefix = "data:image/png;base64,";
  if (!dataUrl.startsWith(pngPrefix)) {
    throw new Error(
      "Screenshot encoding returned an unexpected data URL format",
    );
  }
  const data = dataUrl.slice(pngPrefix.length).trim();
  if (!data) {
    throw new Error("Screenshot encoding produced empty image data");
  }
  if (!/^[A-Za-z0-9+/=]+$/.test(data)) {
    throw new Error("Screenshot encoding produced invalid base64 image data");
  }

  return {
    data,
    mime_type: "image/png",
    width: outputWidth,
    height: outputHeight,
    scale,
    selector: selectorOut,
    index: indexOut,
    timestamp_ms: Date.now(),
  };
}

async function executeAction(
  action: string,
  params: Record<string, unknown>,
): Promise<unknown> {
  switch (action) {
    case "query_elements":
      return handleQueryElements(params);
    case "get_text":
      return handleGetText(params);
    case "list_testids":
      return handleListTestIds(params);
    case "click":
      return handleClick(params);
    case "type":
      return handleType(params);
    case "keypress":
      return handleKeypress(params);
    case "scroll":
      return handleScroll(params);
    case "wait_for":
      return handleWaitFor(params);
    case "capture_screenshot":
      return handleCaptureScreenshot(params);
    default:
      throw new Error(`Unsupported debug UI action '${action}'`);
  }
}

export async function registerDebugUiBridge(): Promise<UnlistenFn> {
  return listen<DebugUiRequestPayload>(
    "tyde-debug-ui-request",
    async (event) => {
      const payload = event.payload;
      if (!payload || typeof payload !== "object") return;

      const action = asString(payload.action);
      if (!action) {
        await submitDebugUiResponse(
          payload.request_id,
          false,
          null,
          "Invalid debug UI action payload",
        );
        return;
      }

      const timeoutMs = normalizeTimeoutMs(asObject(payload.params).timeout_ms);
      try {
        const result = await Promise.race([
          executeAction(action, asObject(payload.params)),
          new Promise<never>((_, reject) => {
            setTimeout(
              () => reject(new Error(`Action timed out after ${timeoutMs}ms`)),
              timeoutMs,
            );
          }),
        ]);
        await submitDebugUiResponse(payload.request_id, true, result);
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        await submitDebugUiResponse(payload.request_id, false, null, message);
      }
    },
  );
}
