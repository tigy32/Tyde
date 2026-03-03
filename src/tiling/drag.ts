import type { DropPosition } from "./types";

let globalDragEndRegistered = false;

export function setupDragSource(tabEl: HTMLElement, panelId: string): void {
  tabEl.draggable = true;

  tabEl.addEventListener("dragstart", (e: DragEvent) => {
    e.dataTransfer!.setData("text/plain", panelId);
    e.dataTransfer!.effectAllowed = "move";
  });

  tabEl.addEventListener("dragend", () => {
    clearAllDropIndicators();
  });
}

export function setupDropTarget(
  leafEl: HTMLElement,
  leafId: string,
  onDrop: (
    panelId: string,
    targetLeafId: string,
    position: DropPosition,
  ) => void,
): void {
  leafEl.addEventListener("dragover", (e: DragEvent) => {
    e.preventDefault();
    e.dataTransfer!.dropEffect = "move";
    const position = computeDropPosition(e, leafEl);
    showDropIndicator(leafEl, position);
  });

  leafEl.addEventListener("dragleave", (e: DragEvent) => {
    // Only clear when mouse actually leaves the leaf boundary
    if (leafEl.contains(e.relatedTarget as Node)) return;
    clearDropIndicator(leafEl);
  });

  leafEl.addEventListener("drop", (e: DragEvent) => {
    e.preventDefault();
    const panelId = e.dataTransfer!.getData("text/plain");
    if (!panelId) return;

    // Prevent dropping a panel onto its own leaf
    const ownedTabs = leafEl.querySelectorAll("[data-panel-id]");
    for (const tab of ownedTabs) {
      if ((tab as HTMLElement).dataset.panelId === panelId) return;
    }

    const position = computeDropPosition(e, leafEl);
    clearDropIndicator(leafEl);
    onDrop(panelId, leafId, position);
  });

  if (!globalDragEndRegistered) {
    globalDragEndRegistered = true;
    document.addEventListener("dragend", () => {
      clearAllDropIndicators();
    });
  }
}

export function computeDropPosition(
  e: DragEvent,
  leafEl: HTMLElement,
): DropPosition {
  const rect = leafEl.getBoundingClientRect();
  const xFraction = (e.clientX - rect.left) / rect.width;
  const yFraction = (e.clientY - rect.top) / rect.height;

  // Edge zones (25% insets) take priority over center
  if (xFraction < 0.25) return "left";
  if (xFraction > 0.75) return "right";
  if (yFraction < 0.25) return "top";
  if (yFraction > 0.75) return "bottom";
  return "center";
}

export function showDropIndicator(
  leafEl: HTMLElement,
  position: DropPosition,
): void {
  leafEl.style.position = leafEl.style.position || "relative";

  let indicator = leafEl.querySelector(
    ".tiling-drop-indicator",
  ) as HTMLElement | null;
  if (!indicator) {
    indicator = document.createElement("div");
    indicator.className = "tiling-drop-indicator";
    leafEl.appendChild(indicator);
  }

  applyIndicatorPosition(indicator, position);
}

export function clearDropIndicator(leafEl: HTMLElement): void {
  const indicator = leafEl.querySelector(".tiling-drop-indicator");
  if (indicator) indicator.remove();
}

export function clearAllDropIndicators(): void {
  const indicators = document.querySelectorAll(".tiling-drop-indicator");
  for (const el of indicators) {
    el.remove();
  }
}

function applyIndicatorPosition(
  indicator: HTMLElement,
  position: DropPosition,
): void {
  const styles: Record<
    DropPosition,
    { top: string; left: string; width: string; height: string }
  > = {
    left: { top: "0", left: "0", width: "50%", height: "100%" },
    right: { top: "0", left: "50%", width: "50%", height: "100%" },
    top: { top: "0", left: "0", width: "100%", height: "50%" },
    bottom: { top: "50%", left: "0", width: "100%", height: "50%" },
    center: { top: "25%", left: "25%", width: "50%", height: "50%" },
  };

  const s = styles[position];
  indicator.style.top = s.top;
  indicator.style.left = s.left;
  indicator.style.width = s.width;
  indicator.style.height = s.height;
}
