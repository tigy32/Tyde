import type { TilingNode } from "./types";

function storageKey(workspacePath: string): string {
  return `tiling-layout:${workspacePath}`;
}

export function saveLayout(workspacePath: string, root: TilingNode): void {
  localStorage.setItem(storageKey(workspacePath), JSON.stringify(root));
}

export function loadLayout(workspacePath: string): TilingNode | null {
  const raw = localStorage.getItem(storageKey(workspacePath));
  if (!raw) return null;

  try {
    const parsed = JSON.parse(raw);
    if (parsed?.kind !== "split" && parsed?.kind !== "leaf") return null;
    return parsed as TilingNode;
  } catch (e) {
    console.error("Failed to parse saved tiling layout", e);
    return null;
  }
}

export function clearLayout(workspacePath: string): void {
  localStorage.removeItem(storageKey(workspacePath));
}
