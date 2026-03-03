import type {
  DropPosition,
  LeafNode,
  PanelTab,
  PanelType,
  SplitNode,
  TilingNode,
} from "./types";
import { generateNodeId } from "./types";

export function findLeafByPanelId(
  root: TilingNode,
  panelId: string,
): LeafNode | null {
  if (root.kind === "leaf") {
    return root.tabs.some((t) => t.id === panelId) ? root : null;
  }
  return (
    findLeafByPanelId(root.first, panelId) ??
    findLeafByPanelId(root.second, panelId)
  );
}

export function findLeafByPanelType(
  root: TilingNode,
  panelType: PanelType,
): LeafNode | null {
  if (root.kind === "leaf") {
    return root.tabs.some((t) => t.type === panelType) ? root : null;
  }
  return (
    findLeafByPanelType(root.first, panelType) ??
    findLeafByPanelType(root.second, panelType)
  );
}

export function findNodeById(
  root: TilingNode,
  nodeId: string,
): TilingNode | null {
  if (root.id === nodeId) return root;
  if (root.kind === "leaf") return null;
  return findNodeById(root.first, nodeId) ?? findNodeById(root.second, nodeId);
}

export function findParent(root: TilingNode, nodeId: string): SplitNode | null {
  if (root.kind === "leaf") return null;
  if (root.first.id === nodeId || root.second.id === nodeId) return root;
  return findParent(root.first, nodeId) ?? findParent(root.second, nodeId);
}

export function addTabToLeaf(leaf: LeafNode, tab: PanelTab): void {
  leaf.tabs.push(tab);
  leaf.activeTabIndex = leaf.tabs.length - 1;
}

export function removeTabFromLeaf(
  leaf: LeafNode,
  panelId: string,
): PanelTab | null {
  const idx = leaf.tabs.findIndex((t) => t.id === panelId);
  if (idx === -1) return null;

  const [removed] = leaf.tabs.splice(idx, 1);
  if (leaf.tabs.length === 0) {
    leaf.activeTabIndex = 0;
    return removed;
  }
  if (leaf.activeTabIndex >= leaf.tabs.length) {
    leaf.activeTabIndex = leaf.tabs.length - 1;
  }
  return removed;
}

function directionForPosition(
  position: DropPosition,
): "horizontal" | "vertical" {
  if (position === "left" || position === "right") return "horizontal";
  return "vertical";
}

export function splitAtLeaf(
  root: TilingNode,
  leafId: string,
  newTab: PanelTab,
  position: DropPosition,
): TilingNode {
  const leaf = findNodeById(root, leafId);
  if (!leaf || leaf.kind !== "leaf") return root;

  if (position === "center") {
    addTabToLeaf(leaf, newTab);
    return root;
  }

  const newLeaf: LeafNode = {
    kind: "leaf",
    id: generateNodeId(),
    tabs: [newTab],
    activeTabIndex: 0,
  };

  const newTabFirst = position === "left" || position === "top";
  const splitNode: SplitNode = {
    kind: "split",
    id: generateNodeId(),
    direction: directionForPosition(position),
    ratio: 0.5,
    first: newTabFirst ? newLeaf : leaf,
    second: newTabFirst ? leaf : newLeaf,
  };

  if (root.id === leafId) return splitNode;

  const parent = findParent(root, leafId);
  if (!parent) return root;

  if (parent.first.id === leafId) {
    parent.first = splitNode;
  } else {
    parent.second = splitNode;
  }
  return root;
}

export function removeLeaf(
  root: TilingNode,
  leafId: string,
): TilingNode | null {
  if (root.id === leafId) return null;
  if (root.kind === "leaf") return root;

  const parent = findParent(root, leafId);
  if (!parent) return root;

  const survivor = parent.first.id === leafId ? parent.second : parent.first;

  if (root.id === parent.id) return survivor;

  // Collapse degenerate single-child splits to maintain balanced structure
  const grandparent = findParent(root, parent.id);
  if (!grandparent) return root;

  if (grandparent.first.id === parent.id) {
    grandparent.first = survivor;
  } else {
    grandparent.second = survivor;
  }
  return root;
}

export function movePanel(
  root: TilingNode,
  panelId: string,
  targetLeafId: string,
  position: DropPosition,
): TilingNode | null {
  const sourceLeaf = findLeafByPanelId(root, panelId);
  if (!sourceLeaf) return root;

  if (sourceLeaf.id === targetLeafId && position === "center") return root;

  const tab = removeTabFromLeaf(sourceLeaf, panelId);
  if (!tab) return root;

  // Tree invariant: no empty leaf containers
  let newRoot: TilingNode | null = root;
  if (sourceLeaf.tabs.length === 0) {
    newRoot = removeLeaf(root, sourceLeaf.id);
    if (!newRoot) {
      return {
        kind: "leaf",
        id: generateNodeId(),
        tabs: [tab],
        activeTabIndex: 0,
      };
    }
  }

  return splitAtLeaf(newRoot, targetLeafId, tab, position);
}

const REQUIRED_PANELS: PanelType[] = ["chat", "git", "explorer", "diff"];

function isPanelType(value: unknown): value is PanelType {
  return (
    value === "chat" ||
    value === "git" ||
    value === "explorer" ||
    value === "diff" ||
    value === "settings" ||
    value === "sessions" ||
    value === "agents" ||
    value === "terminal"
  );
}

function defaultTitleForPanel(type: PanelType): string {
  if (type === "chat") return "Chat";
  if (type === "git") return "Git";
  if (type === "explorer") return "Files";
  if (type === "diff") return "Diff";
  if (type === "settings") return "Settings";
  if (type === "sessions") return "Sessions";
  if (type === "agents") return "Agents";
  if (type === "terminal") return "Terminal";
  return type;
}

function normalizeLeaf(
  raw: Partial<LeafNode> | null | undefined,
): LeafNode | null {
  if (!raw || !Array.isArray(raw.tabs)) return null;

  const tabs: PanelTab[] = [];
  const seenIds = new Set<string>();
  for (const tab of raw.tabs) {
    if (!tab || !isPanelType((tab as Partial<PanelTab>).type)) continue;
    const rawId = (tab as Partial<PanelTab>).id;
    const id = typeof rawId === "string" && rawId ? rawId : generateNodeId();
    if (seenIds.has(id)) continue;
    seenIds.add(id);

    const type = (tab as Partial<PanelTab>).type as PanelType;
    const rawTitle = (tab as Partial<PanelTab>).title;
    const title =
      typeof rawTitle === "string" && rawTitle.trim()
        ? rawTitle
        : defaultTitleForPanel(type);
    tabs.push({ id, type, title });
  }

  if (tabs.length === 0) return null;

  const rawActive = Number(raw.activeTabIndex);
  const activeTabIndex = Number.isFinite(rawActive)
    ? Math.max(0, Math.min(tabs.length - 1, Math.floor(rawActive)))
    : 0;

  return {
    kind: "leaf",
    id: typeof raw.id === "string" && raw.id ? raw.id : generateNodeId(),
    tabs,
    activeTabIndex,
  };
}

function normalizeNode(raw: TilingNode | null | undefined): TilingNode | null {
  if (!raw || typeof raw !== "object") return null;
  if (raw.kind === "leaf") {
    return normalizeLeaf(raw);
  }
  if (raw.kind !== "split") return null;

  const first = normalizeNode(raw.first);
  const second = normalizeNode(raw.second);
  if (!first && !second) return null;
  if (!first) return second;
  if (!second) return first;

  const direction =
    raw.direction === "horizontal" || raw.direction === "vertical"
      ? raw.direction
      : "horizontal";
  const rawRatio = Number(raw.ratio);
  const ratio = Number.isFinite(rawRatio)
    ? Math.max(0.15, Math.min(0.85, rawRatio))
    : 0.5;

  return {
    kind: "split",
    id: typeof raw.id === "string" && raw.id ? raw.id : generateNodeId(),
    direction,
    ratio,
    first,
    second,
  };
}

function walkLeaves(node: TilingNode, visit: (leaf: LeafNode) => void): void {
  if (node.kind === "leaf") {
    visit(node);
    return;
  }
  walkLeaves(node.first, visit);
  walkLeaves(node.second, visit);
}

function collectPanelCounts(root: TilingNode): Map<PanelType, number> {
  const counts = new Map<PanelType, number>();
  walkLeaves(root, (leaf) => {
    for (const tab of leaf.tabs) {
      counts.set(tab.type, (counts.get(tab.type) ?? 0) + 1);
    }
  });
  return counts;
}

function hasTabbedRightLeaf(root: TilingNode): boolean {
  let found = false;
  walkLeaves(root, (leaf) => {
    if (found) return;
    const types = new Set(leaf.tabs.map((tab) => tab.type));
    if (types.has("git") && types.has("explorer") && types.has("diff")) {
      found = true;
    }
  });
  return found;
}

export function isLayoutSane(root: TilingNode): boolean {
  const counts = collectPanelCounts(root);
  for (const panel of REQUIRED_PANELS) {
    if ((counts.get(panel) ?? 0) < 1) return false;
  }
  if (!hasTabbedRightLeaf(root)) return false;
  return true;
}

function hasPanelType(leaf: LeafNode, panelType: PanelType): boolean {
  return leaf.tabs.some((tab) => tab.type === panelType);
}

function mergeRightPanels(topLeaf: LeafNode, bottomLeaf: LeafNode): LeafNode {
  const mergedTabs = [...topLeaf.tabs];
  const knownTabIds = new Set(mergedTabs.map((tab) => tab.id));
  for (const tab of bottomLeaf.tabs) {
    if (tab.type !== "diff") continue;
    if (knownTabIds.has(tab.id)) continue;
    knownTabIds.add(tab.id);
    mergedTabs.push(tab);
  }

  const activeTopTab = topLeaf.tabs[topLeaf.activeTabIndex];
  let activeTabIndex = mergedTabs.findIndex(
    (tab) => tab.id === activeTopTab?.id,
  );
  if (activeTabIndex < 0) activeTabIndex = 0;

  return {
    kind: "leaf",
    id: topLeaf.id,
    tabs: mergedTabs,
    activeTabIndex,
  };
}

// Legacy layouts used a right-side vertical split where Files/Git were on top and Diff
// was forced into a lower leaf. This migration flattens that into right-side tabs.
export function migrateLegacyLayout(root: TilingNode): TilingNode {
  if (root.kind === "leaf") return root;

  root.first = migrateLegacyLayout(root.first);
  root.second = migrateLegacyLayout(root.second);

  if (root.direction !== "horizontal") return root;
  if (root.first.kind !== "leaf") return root;
  if (root.second.kind !== "split") return root;
  if (root.second.direction !== "vertical") return root;
  if (root.second.first.kind !== "leaf" || root.second.second.kind !== "leaf")
    return root;

  const leftLeaf = root.first;
  const topRightLeaf = root.second.first;
  const bottomRightLeaf = root.second.second;
  if (!hasPanelType(leftLeaf, "chat")) return root;
  if (!hasPanelType(topRightLeaf, "git")) return root;
  if (!hasPanelType(topRightLeaf, "explorer")) return root;
  if (
    !hasPanelType(bottomRightLeaf, "diff") &&
    !hasPanelType(topRightLeaf, "diff")
  )
    return root;

  root.second = mergeRightPanels(topRightLeaf, bottomRightLeaf);
  return root;
}

export function normalizeLayout(
  root: TilingNode | null | undefined,
): TilingNode {
  const normalized = normalizeNode(root);
  if (!normalized) return createDefaultLayout();

  const migrated = migrateLegacyLayout(normalized);
  if (!isLayoutSane(migrated)) {
    return createDefaultLayout();
  }
  return migrated;
}

export function createDefaultLayout(): TilingNode {
  return {
    kind: "split",
    id: generateNodeId(),
    direction: "horizontal",
    ratio: 0.65,
    first: {
      kind: "leaf",
      id: generateNodeId(),
      tabs: [{ id: generateNodeId(), type: "chat", title: "Chat" }],
      activeTabIndex: 0,
    },
    second: {
      kind: "leaf",
      id: generateNodeId(),
      tabs: [
        { id: generateNodeId(), type: "git", title: "Git" },
        { id: generateNodeId(), type: "explorer", title: "Files" },
        { id: generateNodeId(), type: "diff", title: "Diff" },
      ],
      activeTabIndex: 0,
    },
  };
}
