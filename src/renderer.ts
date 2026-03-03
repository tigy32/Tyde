import { diffLines } from "diff";
import hljs from "highlight.js/lib/core";
import bash from "highlight.js/lib/languages/bash";
import css from "highlight.js/lib/languages/css";
import diffLang from "highlight.js/lib/languages/diff";
import ini from "highlight.js/lib/languages/ini";
import javascript from "highlight.js/lib/languages/javascript";
import json from "highlight.js/lib/languages/json";
import markdown from "highlight.js/lib/languages/markdown";
import python from "highlight.js/lib/languages/python";
import rust from "highlight.js/lib/languages/rust";
import sql from "highlight.js/lib/languages/sql";
import typescript from "highlight.js/lib/languages/typescript";
import xml from "highlight.js/lib/languages/xml";
import yaml from "highlight.js/lib/languages/yaml";
import { Marked, Renderer, type Tokens } from "marked";

hljs.registerLanguage("typescript", typescript);
hljs.registerLanguage("javascript", javascript);
hljs.registerLanguage("rust", rust);
hljs.registerLanguage("python", python);
hljs.registerLanguage("json", json);
hljs.registerLanguage("bash", bash);
hljs.registerLanguage("xml", xml);
hljs.registerLanguage("css", css);
hljs.registerLanguage("yaml", yaml);
hljs.registerLanguage("ini", ini);
hljs.registerLanguage("sql", sql);

hljs.registerLanguage("markdown", markdown);
hljs.registerLanguage("diff", diffLang);

hljs.registerLanguage("ts", typescript);
hljs.registerLanguage("js", javascript);
hljs.registerLanguage("py", python);
hljs.registerLanguage("sh", bash);
hljs.registerLanguage("shell", bash);
hljs.registerLanguage("html", xml);
hljs.registerLanguage("toml", ini);

export function escapeHtml(text: string): string {
  return text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#039;");
}

const customRenderer = new Renderer();
let markedInstance: Marked;

const SAFE_LINK_PROTOCOLS = new Set(["http:", "https:", "mailto:"]);
const SAFE_ROOTED_LINK_PREFIXES = ["#", "/", "./", "../"];

const ALLOWED_RENDER_TAGS = new Set([
  "a",
  "blockquote",
  "br",
  "code",
  "del",
  "details",
  "div",
  "em",
  "h1",
  "h2",
  "h3",
  "h4",
  "h5",
  "h6",
  "hr",
  "li",
  "ol",
  "p",
  "pre",
  "span",
  "strong",
  "summary",
  "table",
  "tbody",
  "td",
  "th",
  "thead",
  "tr",
  "ul",
]);

const ALLOWED_RENDER_ATTRS = new Set([
  "class",
  "data-code",
  "data-lang",
  "href",
  "rel",
  "style",
  "target",
]);

customRenderer.code = ({ text, lang }: { text: string; lang?: string }) => {
  const encoded = btoa(
    Array.from(new TextEncoder().encode(text), (b) =>
      String.fromCharCode(b),
    ).join(""),
  );
  const langLabel = lang
    ? `<span class="code-lang">${escapeHtml(lang)}</span>`
    : "";
  const dataLang = lang ? ` data-lang="${escapeHtml(lang)}"` : "";

  let highlighted: string;
  if (lang && hljs.getLanguage(lang)) {
    highlighted = hljs.highlight(text, { language: lang }).value;
  } else {
    highlighted = hljs.highlightAuto(text).value;
  }

  return `<div class="code-block-container"><div class="code-block"><div class="code-header">${langLabel}<button class="copy-btn" data-code="${encoded}">Copy</button></div><pre><code class="hljs"${dataLang}>${highlighted}</code></pre></div></div>`;
};

customRenderer.codespan = ({ text }: { text: string }) => {
  return `<code class="inline-code">${escapeHtml(text)}</code>`;
};

customRenderer.link = ({ href, text }: { href: string; text: string }) => {
  const safeHref = sanitizeHref(href);
  if (!safeHref) return `<span class="external-link">${text}</span>`;
  return `<a href="${escapeHtml(safeHref)}" target="_blank" rel="noopener noreferrer" class="external-link">${text}</a>`;
};

function renderInlineMarkdown(text: string): string {
  const rendered = markedInstance.parseInline(text) as string;
  return rendered;
}

customRenderer.heading = ({ text, depth }: { text: string; depth: number }) => {
  return `<h${depth}>${renderInlineMarkdown(text)}</h${depth}>`;
};

customRenderer.table = (token: Tokens.Table) => {
  const headerCells = token.header
    .map((cell) => {
      const align = cell.align ? ` style="text-align:${cell.align}"` : "";
      return `<th${align}>${cell.text}</th>`;
    })
    .join("");
  const headerRow = `<tr>${headerCells}</tr>`;

  const bodyRows = token.rows
    .map((row) => {
      const cells = row
        .map((cell) => {
          const align = cell.align ? ` style="text-align:${cell.align}"` : "";
          return `<td${align}>${cell.text}</td>`;
        })
        .join("");
      return `<tr>${cells}</tr>`;
    })
    .join("");

  return `<table class="md-table"><thead>${headerRow}</thead><tbody>${bodyRows}</tbody></table>`;
};

customRenderer.hr = () => {
  return `<hr class="md-hr">`;
};

customRenderer.listitem = ({
  text,
  checked,
}: {
  text: string;
  checked?: boolean;
}) => {
  const inline = renderInlineMarkdown(text);
  if (typeof checked === "boolean") {
    if (checked) {
      return `<li class="task-item"><span class="task-checkbox task-checked">☑</span> ${inline}</li>`;
    }
    return `<li class="task-item"><span class="task-checkbox task-unchecked">☐</span> ${inline}</li>`;
  }
  return `<li>${inline}</li>`;
};

customRenderer.blockquote = ({ text }: { text: string }) => {
  return `<blockquote>${renderInlineMarkdown(text)}</blockquote>`;
};

customRenderer.paragraph = ({ text }: { text: string }) => {
  return `<p>${renderInlineMarkdown(text)}</p>`;
};

customRenderer.html = ({ text }: { text: string }) => {
  return escapeHtml(text);
};

markedInstance = new Marked({
  renderer: customRenderer,
  breaks: true,
  gfm: true,
});

export function renderContent(text: string): string {
  const rendered = markedInstance.parse(text) as string;
  return sanitizeRenderedHtml(rendered);
}

export function renderDiff(
  before: string,
  after: string,
  filePath?: string,
): string {
  const diff = diffLines(before, after);

  let header = "";
  if (filePath) {
    header = `<div class="diff-file-header"><code>${escapeHtml(filePath)}</code></div>`;
  }

  let oldLine = 1;
  let newLine = 1;

  const lines: string[] = [];
  for (const part of diff) {
    const rawLines = part.value.split("\n");
    if (rawLines.length > 0 && rawLines[rawLines.length - 1] === "") {
      rawLines.pop();
    }

    const type: DiffEntry["type"] = part.added
      ? "added"
      : part.removed
        ? "removed"
        : "unchanged";
    for (const textLine of rawLines) {
      const escaped = escapeHtml(textLine);
      if (type === "removed") {
        const ln = oldLine++;
        lines.push(
          `<div class="diff-line diff-removed"><span class="diff-line-num diff-line-old">${ln}</span><span class="diff-line-num"></span><span class="diff-marker">-</span><span class="diff-line-text">${escaped}</span></div>`,
        );
        continue;
      }
      if (type === "added") {
        const ln = newLine++;
        lines.push(
          `<div class="diff-line diff-added"><span class="diff-line-num"></span><span class="diff-line-num diff-line-new">${ln}</span><span class="diff-marker">+</span><span class="diff-line-text">${escaped}</span></div>`,
        );
        continue;
      }
      const oLn = oldLine++;
      const nLn = newLine++;
      lines.push(
        `<div class="diff-line diff-unchanged"><span class="diff-line-num">${oLn}</span><span class="diff-line-num">${nLn}</span><span class="diff-marker"> </span><span class="diff-line-text">${escaped}</span></div>`,
      );
    }
  }

  return `${header}<div class="diff-view">${lines.join("")}</div>`;
}

interface DiffEntry {
  type: "added" | "removed" | "unchanged";
}

function sanitizeHref(rawHref: string): string | null {
  const href = String(rawHref ?? "").trim();
  if (!href) return null;

  if (SAFE_ROOTED_LINK_PREFIXES.some((prefix) => href.startsWith(prefix))) {
    return href;
  }

  try {
    const base = globalThis.location?.origin ?? "https://tyde.local";
    const parsed = new URL(href, base);
    if (!SAFE_LINK_PROTOCOLS.has(parsed.protocol)) return null;
    return parsed.toString();
  } catch {
    return null;
  }
}

function sanitizeStyle(style: string): string | null {
  const declarations = style
    .split(";")
    .map((part) => part.trim())
    .filter(Boolean);

  const sanitized: string[] = [];
  for (const declaration of declarations) {
    const idx = declaration.indexOf(":");
    if (idx === -1) continue;

    const prop = declaration.slice(0, idx).trim().toLowerCase();
    const value = declaration
      .slice(idx + 1)
      .trim()
      .toLowerCase();
    if (prop !== "text-align") continue;
    if (!["left", "right", "center", "justify", "start", "end"].includes(value))
      continue;
    sanitized.push(`text-align:${value}`);
  }

  if (sanitized.length === 0) return null;
  return sanitized.join(";");
}

function sanitizeRenderedHtml(html: string): string {
  const template = document.createElement("template");
  template.innerHTML = html;

  const out = document.createElement("div");
  const sanitizeNode = (node: Node): Node[] => {
    if (node.nodeType === Node.TEXT_NODE) {
      return [document.createTextNode(node.textContent ?? "")];
    }

    if (node.nodeType !== Node.ELEMENT_NODE) return [];
    const el = node as HTMLElement;
    const tag = el.tagName.toLowerCase();

    if (!ALLOWED_RENDER_TAGS.has(tag)) {
      const unwrapped: Node[] = [];
      for (const child of Array.from(el.childNodes)) {
        unwrapped.push(...sanitizeNode(child));
      }
      return unwrapped;
    }

    const clean = document.createElement(tag);
    for (const attr of Array.from(el.attributes)) {
      const name = attr.name.toLowerCase();
      if (name.startsWith("on")) continue;
      if (!ALLOWED_RENDER_ATTRS.has(name)) continue;

      if (name === "href") {
        const safeHref = sanitizeHref(attr.value);
        if (!safeHref) continue;
        clean.setAttribute("href", safeHref);
        clean.setAttribute("target", "_blank");
        clean.setAttribute("rel", "noopener noreferrer");
        continue;
      }

      if (name === "target") {
        clean.setAttribute("target", "_blank");
        continue;
      }

      if (name === "rel") {
        clean.setAttribute("rel", "noopener noreferrer");
        continue;
      }

      if (name === "style") {
        const safeStyle = sanitizeStyle(attr.value);
        if (!safeStyle) continue;
        clean.setAttribute("style", safeStyle);
        continue;
      }

      clean.setAttribute(attr.name, attr.value);
    }

    for (const child of Array.from(el.childNodes)) {
      for (const sanitizedChild of sanitizeNode(child)) {
        clean.appendChild(sanitizedChild);
      }
    }
    return [clean];
  };

  for (const child of Array.from(template.content.childNodes)) {
    for (const sanitized of sanitizeNode(child)) {
      out.appendChild(sanitized);
    }
  }
  return out.innerHTML;
}

export function wrapWithTruncation(
  html: string,
  rawLength: number,
  threshold: number = 2000,
): string {
  if (rawLength <= threshold) return html;

  return `<div class="truncatable collapsed">
    <div class="truncatable-content">${html}</div>
    <div class="truncatable-fade"></div>
    <button class="truncatable-toggle" onclick="
      const container = this.closest('.truncatable');
      if (container.classList.contains('collapsed')) {
        container.classList.remove('collapsed');
        this.textContent = 'Show less';
      } else {
        container.classList.add('collapsed');
        this.textContent = 'Show more';
      }
    ">Show more</button>
  </div>`;
}

export function renderCommandOutput(
  stdout: string,
  stderr: string,
  exitCode: number,
): string {
  const exitClass = exitCode === 0 ? "exit-success" : "exit-failure";
  const exitLabel = `<span class="exit-code ${exitClass}">exit: ${exitCode}</span>`;

  let body = "";
  if (stdout) {
    body += `<div class="stdout">${escapeHtml(stdout)}</div>`;
  }
  if (stderr) {
    body += `<div class="stderr">${escapeHtml(stderr)}</div>`;
  }

  return `<div class="command-output">${exitLabel}${body}</div>`;
}
