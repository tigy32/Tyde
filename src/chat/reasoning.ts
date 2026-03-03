function stripLineMarkdownWrappers(line: string): string {
  let current = line.trim();
  while (true) {
    if (
      current.length > 4 &&
      current.startsWith("**") &&
      current.endsWith("**")
    ) {
      current = current.slice(2, -2).trim();
      continue;
    }
    if (
      current.length > 4 &&
      current.startsWith("__") &&
      current.endsWith("__")
    ) {
      current = current.slice(2, -2).trim();
      continue;
    }
    if (
      current.length > 2 &&
      current.startsWith("`") &&
      current.endsWith("`")
    ) {
      current = current.slice(1, -1).trim();
      continue;
    }
    return current;
  }
}

function stripInlineMarkdownArtifacts(line: string): string {
  return line
    .replace(/(?:\*{2,}|_{2,}|`{1,3})/g, " ")
    .replace(/\s{2,}/g, " ")
    .trim();
}

function collapseRepeatedWordArtifacts(line: string): string {
  let current = line;
  for (let i = 0; i < 4; i += 1) {
    const next = current
      .replace(/\b([a-z][a-z0-9_-]{2,})(?:\s+\1\b)+/gi, "$1")
      .replace(/([a-z]{3,})\1+/gi, "$1");
    if (next === current) break;
    current = next;
  }
  return current;
}

function lineDedupKey(line: string): string {
  return line.toLowerCase().replace(/[^a-z0-9]+/g, "");
}

function lineScore(line: string): number {
  const words = line.split(/\s+/).filter(Boolean).length;
  // Prefer lines with clearer word boundaries when replacing duplicates.
  return words * 100 + line.length;
}

export function normalizeReasoningText(text: string): string {
  if (typeof text !== "string" || text.trim().length === 0) return "";

  const normalized = text
    .replace(/\r/g, "")
    .replace(/[\u200B-\u200D\uFEFF]/g, "");
  const lines = normalized.split("\n");
  const out: string[] = [];
  const keyToIndex = new Map<string, number>();

  for (const rawLine of lines) {
    const trimmed = rawLine.trim();
    if (trimmed.length === 0) {
      if (out.length > 0 && out[out.length - 1] !== "") {
        out.push("");
      }
      continue;
    }

    const cleanedLine = collapseRepeatedWordArtifacts(
      stripInlineMarkdownArtifacts(stripLineMarkdownWrappers(trimmed)),
    );
    if (cleanedLine.length === 0) continue;
    const key = lineDedupKey(cleanedLine);

    if (key.length < 10) {
      out.push(cleanedLine);
      continue;
    }

    const existingIdx = keyToIndex.get(key);
    if (existingIdx === undefined) {
      keyToIndex.set(key, out.length);
      out.push(cleanedLine);
      continue;
    }

    if (lineScore(cleanedLine) > lineScore(out[existingIdx])) {
      out[existingIdx] = cleanedLine;
    }
  }

  while (out.length > 0 && out[out.length - 1] === "") {
    out.pop();
  }

  return out.join("\n").trim();
}
