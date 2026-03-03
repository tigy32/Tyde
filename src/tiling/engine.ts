import { setupDragSource, setupDropTarget } from "./drag";
import { loadLayout, saveLayout } from "./persistence";
import { renderTree } from "./renderer";
import {
  addTabToLeaf,
  createDefaultLayout,
  findLeafByPanelType,
  findNodeById,
  movePanel,
  normalizeLayout,
  splitAtLeaf,
} from "./tree";
import type {
  DropPosition,
  LeafNode,
  PanelFactory,
  PanelTab,
  PanelType,
  TilingNode,
} from "./types";
import { generateNodeId } from "./types";

export class TilingEngine {
  private root: TilingNode;
  private container: HTMLElement;
  private panelFactory: PanelFactory;
  private workspacePath: string;

  constructor(
    container: HTMLElement,
    panelFactory: PanelFactory,
    workspacePath: string,
  ) {
    this.container = container;
    this.panelFactory = panelFactory;
    this.workspacePath = workspacePath;
    const loadedLayout = loadLayout(workspacePath);
    this.root = loadedLayout
      ? normalizeLayout(loadedLayout)
      : createDefaultLayout();
    this.render();
  }

  getRoot(): TilingNode {
    return this.root;
  }

  getLayoutTree(): TilingNode {
    return this.root;
  }

  setLayoutTree(tree: TilingNode): void {
    this.root = normalizeLayout(tree);
    this.render();
  }

  render(): void {
    renderTree(
      this.root,
      this.container,
      this.panelFactory,
      this.handleRatioChange.bind(this),
    );

    const leafEls =
      this.container.querySelectorAll<HTMLElement>(".tiling-leaf");
    for (const leafEl of leafEls) {
      const nodeId = leafEl.dataset.nodeId;
      if (!nodeId) continue;
      setupDropTarget(leafEl, nodeId, this.handleDrop.bind(this));
    }

    const tabEls = this.container.querySelectorAll<HTMLElement>(".tiling-tab");
    for (const tabEl of tabEls) {
      const panelId = tabEl.dataset.panelId;
      if (!panelId) continue;
      setupDragSource(tabEl, panelId);
    }

    saveLayout(this.workspacePath, this.root);
  }

  switchToPanel(panelType: PanelType): void {
    const leaf = findLeafByPanelType(this.root, panelType);
    if (!leaf) return;

    const idx = leaf.tabs.findIndex((t) => t.type === panelType);
    if (idx === -1) return;

    leaf.activeTabIndex = idx;
    this.render();
  }

  ensurePanelVisible(panelType: PanelType): void {
    const existing = findLeafByPanelType(this.root, panelType);
    if (existing) {
      const idx = existing.tabs.findIndex((t) => t.type === panelType);
      if (idx !== -1) existing.activeTabIndex = idx;
      this.render();
      return;
    }

    const tab: PanelTab = {
      id: generateNodeId(),
      type: panelType,
      title: panelType,
    };
    const targetLeaf = this.findFirstLeaf(this.root);
    addTabToLeaf(targetLeaf, tab);
    this.render();
  }

  addPanel(
    tab: PanelTab,
    targetLeafId?: string,
    position: DropPosition = "center",
  ): void {
    if (targetLeafId) {
      this.root = splitAtLeaf(this.root, targetLeafId, tab, position);
      this.render();
      return;
    }

    const leaf = this.findFirstLeaf(this.root);
    addTabToLeaf(leaf, tab);
    this.render();
  }

  resetLayout(): void {
    this.root = createDefaultLayout();
    this.render();
  }

  private handleRatioChange(nodeId: string, newRatio: number): void {
    const node = findNodeById(this.root, nodeId);
    if (!node || node.kind !== "split") return;
    node.ratio = newRatio;
    saveLayout(this.workspacePath, this.root);
  }

  private handleDrop(
    panelId: string,
    targetLeafId: string,
    position: DropPosition,
  ): void {
    const result = movePanel(this.root, panelId, targetLeafId, position);
    if (result) this.root = result;
    this.render();
  }

  private findFirstLeaf(node: TilingNode): LeafNode {
    if (node.kind === "leaf") return node;
    return this.findFirstLeaf(node.first);
  }
}
