import type { LeafNode, PanelFactory, SplitNode, TilingNode } from "./types";

export function renderTree(
  root: TilingNode,
  container: HTMLElement,
  panelFactory: PanelFactory,
  onRatioChange: (nodeId: string, newRatio: number) => void,
): void {
  container.innerHTML = "";
  const el = renderNode(root, panelFactory, onRatioChange);
  container.appendChild(el);
}

function renderNode(
  node: TilingNode,
  panelFactory: PanelFactory,
  onRatioChange: (nodeId: string, newRatio: number) => void,
): HTMLElement {
  if (node.kind === "leaf") {
    return renderLeaf(node, panelFactory);
  }
  return renderSplit(node, panelFactory, onRatioChange);
}

function renderSplit(
  node: SplitNode,
  panelFactory: PanelFactory,
  onRatioChange: (nodeId: string, newRatio: number) => void,
): HTMLElement {
  const el = document.createElement("div");
  el.className = "tiling-split";
  el.dataset.nodeId = node.id;

  const isHorizontal = node.direction === "horizontal";
  const template = `${node.ratio}fr 6px ${1 - node.ratio}fr`;
  if (isHorizontal) {
    el.style.gridTemplateColumns = template;
  } else {
    el.style.gridTemplateRows = template;
  }

  const firstContainer = document.createElement("div");
  firstContainer.style.overflow = "hidden";
  firstContainer.style.width = "100%";
  firstContainer.style.height = "100%";
  firstContainer.style.minWidth = "0";
  firstContainer.style.minHeight = "0";

  const handle = document.createElement("div");
  handle.className = isHorizontal
    ? "tiling-handle tiling-handle-h"
    : "tiling-handle tiling-handle-v";

  const secondContainer = document.createElement("div");
  secondContainer.style.overflow = "hidden";
  secondContainer.style.width = "100%";
  secondContainer.style.height = "100%";
  secondContainer.style.minWidth = "0";
  secondContainer.style.minHeight = "0";

  el.appendChild(firstContainer);
  el.appendChild(handle);
  el.appendChild(secondContainer);

  firstContainer.appendChild(
    renderNode(node.first, panelFactory, onRatioChange),
  );
  secondContainer.appendChild(
    renderNode(node.second, panelFactory, onRatioChange),
  );

  setupResizeHandle(handle, el, node, onRatioChange);

  return el;
}

function renderLeaf(node: LeafNode, panelFactory: PanelFactory): HTMLElement {
  const el = document.createElement("div");
  el.className = "tiling-leaf";
  el.dataset.nodeId = node.id;

  if (node.tabs.length > 1) {
    el.appendChild(buildTabBar(node, el, panelFactory));
  }

  const contentDiv = document.createElement("div");
  contentDiv.className = "tiling-panel-content";
  el.appendChild(contentDiv);

  const activeTab = node.tabs[node.activeTabIndex];
  if (activeTab) {
    const panel = panelFactory(activeTab.type);
    contentDiv.appendChild(panel);
  }

  return el;
}

function buildTabBar(
  node: LeafNode,
  leafEl: HTMLElement,
  panelFactory: PanelFactory,
): HTMLElement {
  const bar = document.createElement("div");
  bar.className = "tiling-tab-bar";

  for (let i = 0; i < node.tabs.length; i++) {
    const tab = node.tabs[i];
    const tabEl = document.createElement("div");
    tabEl.className = "tiling-tab";
    if (i === node.activeTabIndex) {
      tabEl.classList.add("tiling-tab-active");
    }
    tabEl.textContent = tab.title;
    tabEl.draggable = true;
    tabEl.dataset.panelId = tab.id;

    const index = i;
    tabEl.addEventListener("click", () => {
      node.activeTabIndex = index;
      updateLeafContent(leafEl, node, panelFactory);
    });

    bar.appendChild(tabEl);
  }

  return bar;
}

export function updateLeafContent(
  leafEl: HTMLElement,
  leaf: LeafNode,
  panelFactory: PanelFactory,
): void {
  const contentDiv = leafEl.querySelector(".tiling-panel-content");
  if (!contentDiv) return;

  contentDiv.innerHTML = "";

  const activeTab = leaf.tabs[leaf.activeTabIndex];
  if (activeTab) {
    contentDiv.appendChild(panelFactory(activeTab.type));
  }

  // Toggle active class in-place rather than re-rendering the tab bar, which would destroy drag listeners
  const tabs = leafEl.querySelectorAll(".tiling-tab");
  tabs.forEach((tabEl, i) => {
    tabEl.classList.toggle("tiling-tab-active", i === leaf.activeTabIndex);
  });
}

export function setupResizeHandle(
  handle: HTMLElement,
  splitEl: HTMLElement,
  node: SplitNode,
  onRatioChange: (nodeId: string, newRatio: number) => void,
): void {
  const isHorizontal = node.direction === "horizontal";

  handle.addEventListener("mousedown", (startEvent: MouseEvent) => {
    startEvent.preventDefault();
    handle.classList.add("tiling-handle-active");
    document.body.style.cursor = isHorizontal ? "col-resize" : "row-resize";

    // Text selection during drag creates visual artifacts and confuses resize intent
    document.body.style.userSelect = "none";

    let currentRatio = node.ratio;

    const onMouseMove = (e: MouseEvent) => {
      const rect = splitEl.getBoundingClientRect();

      if (isHorizontal) {
        currentRatio = (e.clientX - rect.left) / rect.width;
      } else {
        currentRatio = (e.clientY - rect.top) / rect.height;
      }

      currentRatio = Math.max(0.15, Math.min(0.85, currentRatio));

      const template = `${currentRatio}fr 6px ${1 - currentRatio}fr`;
      if (isHorizontal) {
        splitEl.style.gridTemplateColumns = template;
      } else {
        splitEl.style.gridTemplateRows = template;
      }
    };

    const onMouseUp = () => {
      handle.classList.remove("tiling-handle-active");
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
      document.removeEventListener("mousemove", onMouseMove);
      document.removeEventListener("mouseup", onMouseUp);
      onRatioChange(node.id, currentRatio);
    };

    document.addEventListener("mousemove", onMouseMove);
    document.addEventListener("mouseup", onMouseUp);
  });
}
