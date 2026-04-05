export type DiffViewMode = "unified" | "side-by-side";
export type DiffContextMode = "hunks" | "full";

export interface DiffSettings {
  viewMode: DiffViewMode;
  contextMode: DiffContextMode;
}

const VIEW_MODE_KEY = "tyde-diff-view-mode";
const CONTEXT_MODE_KEY = "tyde-diff-context-mode";

const DEFAULT_VIEW_MODE: DiffViewMode = "unified";
const DEFAULT_CONTEXT_MODE: DiffContextMode = "hunks";

const diffSettingsUpdateCallbacks = new Set<(settings: DiffSettings) => void>();

export function initDiffSettings(): void {
  if (localStorage.getItem(VIEW_MODE_KEY) === null) {
    localStorage.setItem(VIEW_MODE_KEY, DEFAULT_VIEW_MODE);
  }
  if (localStorage.getItem(CONTEXT_MODE_KEY) === null) {
    localStorage.setItem(CONTEXT_MODE_KEY, DEFAULT_CONTEXT_MODE);
  }
}

export function getDiffSettings(): DiffSettings {
  const viewMode = localStorage.getItem(VIEW_MODE_KEY) as DiffViewMode;
  const contextMode = localStorage.getItem(CONTEXT_MODE_KEY) as DiffContextMode;

  return { viewMode, contextMode };
}

export function setDiffSettings(settings: Partial<DiffSettings>): void {
  if (settings.viewMode !== undefined) {
    localStorage.setItem(VIEW_MODE_KEY, settings.viewMode);
  }
  if (settings.contextMode !== undefined) {
    localStorage.setItem(CONTEXT_MODE_KEY, settings.contextMode);
  }
  broadcastDiffSettings();
}

export function broadcastDiffSettings(): void {
  const current = getDiffSettings();
  for (const cb of diffSettingsUpdateCallbacks) {
    cb(current);
  }
}

export function onDiffSettingsChange(
  cb: (settings: DiffSettings) => void,
): () => void {
  diffSettingsUpdateCallbacks.add(cb);
  return () => {
    diffSettingsUpdateCallbacks.delete(cb);
  };
}
