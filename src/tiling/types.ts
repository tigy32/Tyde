export type SplitDirection = "horizontal" | "vertical";

export type PanelType =
  | "chat"
  | "git"
  | "explorer"
  | "diff"
  | "settings"
  | "sessions"
  | "agents"
  | "terminal"
  | "workflows";

export interface PanelTab {
  id: string;
  type: PanelType;
  title: string;
}

export interface LeafNode {
  kind: "leaf";
  id: string;
  tabs: PanelTab[];
  activeTabIndex: number;
}

export interface SplitNode {
  kind: "split";
  id: string;
  direction: SplitDirection;
  ratio: number;
  first: TilingNode;
  second: TilingNode;
}

export type TilingNode = SplitNode | LeafNode;

export type DropPosition = "left" | "right" | "top" | "bottom" | "center";

export type PanelFactory = (panelType: PanelType) => HTMLElement;

let nodeIdCounter = 0;

export function generateNodeId(): string {
  nodeIdCounter += 1;
  return `node-${nodeIdCounter}-${Date.now()}`;
}
