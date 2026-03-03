# Tyde Architecture: Isolated State Containers

## Core Principle

Every stateful UI entity (workspace, tab, chat session, widget) owns its own DOM subtree and state object. Switching between entities toggles visibility — no teardown, no rebuild, no state synchronization.

## Hierarchy

```
AppController (global shell)
├── NotificationManager        (shared — toasts are global)
├── KeyboardManager            (shared — keybindings are global)
├── CommandPalette             (shared — overlay, workspace-root swaps on switch)
├── SettingsPanel              (shared — settings are global config)
├── GlobalEventDispatcher      (routes conversation_id → WorkspaceView)
│
├── WorkspaceView[project-1]           ← display: block (active)
│   ├── root: HTMLElement              (complete workbench DOM subtree)
│   ├── TabManager                     (own instance, own tab bar DOM)
│   ├── Layout                         (own instance, own workbench DOM)
│   ├── GitPanel                       (own instance, own DOM)
│   ├── FileExplorer                   (own instance, own DOM)
│   ├── SessionsPanel                  (own instance, own DOM)
│   ├── AgentsPanel                    (own instance, own DOM)
│   ├── DiffPanel                      (own instance, own DOM)
│   ├── TaskPanel                      (own instance, own DOM)
│   ├── ChatPanel                      (own instance)
│   │   ├── ConversationView[conv-1]   ← display: block
│   │   ├── ConversationView[conv-2]   ← display: none
│   │   └── ConversationView[conv-3]   ← display: none
│   └── EventRouter                    (own instance, routes to this workspace's components)
│
├── WorkspaceView[project-2]           ← display: none
│   └── ... (complete independent copy)
│
└── HomeView                           ← display: none
```

## Design Tenets

### 1. Isolation by Default
Each `WorkspaceView` is a self-contained unit. It creates its own complete DOM subtree and component instances at construction time. No mutable state is shared between workspace views.

### 2. Visibility Toggling, Not State Sync
Switching workspaces: `activeWorkspace.root.style.display = 'none'` + `newWorkspace.root.style.display = 'block'`. Zero teardown, zero rebuild. The incoming workspace's DOM is already up-to-date because processing never stopped.

### 3. Background Processing Continues
If workspace-2 has a streaming conversation, its ChatPanel keeps processing events and updating its (hidden) DOM. When the user switches back, everything is already rendered and current.

### 4. Event Routing by Ownership
Backend events arrive with `conversation_id`. A global dispatcher maps `conversation_id → WorkspaceView`, then delegates to that workspace's EventRouter. No "is this the active tab?" checks needed for event processing — every event always reaches its owner.

### 5. Each Entity Owns Its DOM
- `WorkspaceView` owns a complete workbench DOM tree
- `ChatPanel` owns per-conversation `ConversationView` containers (already implemented)
- Each widget (git, files, sessions, agents) owns its own container element
- `TabManager` owns its tab bar element
- `DiffPanel` owns its diff viewer element

### 6. Shared Components Are Explicitly Global
Only truly global concerns live at the app level:
- **NotificationManager** — toasts are cross-workspace
- **KeyboardManager** — keybindings are global
- **CommandPalette** — single overlay, but `workspaceRoot` updates on workspace switch
- **SettingsPanel** — global config, not per-workspace

### 7. No Save/Restore Pattern
The `saveCurrentProjectState()` / `applyWorkspaceUI()` pattern is eliminated entirely. Each workspace's state lives permanently in its own DOM and component instances. Persistence to localStorage happens on mutation (when the user changes something), not on workspace switch.

### 8. Single Entry Point for Workspace Switching
All workspace switching goes through one method: `AppController.switchToWorkspace(id)`. This method:
1. Hides the current workspace's root element
2. Shows the target workspace's root element  
3. Updates shared components (command palette workspace root, window title)
4. Updates `activeWorkspaceId`

That's it. No UI state to synchronize, no tabs to restore, no panels to refresh.

## What This Eliminates

- `saveCurrentProjectState()` and `applyWorkspaceUI()` — gone
- `projectRuntimeTabs` Map and tab snapshot save/restore — gone
- Manual `gitPanel.setWorkingDir()` / `fileExplorer.setRootPath()` on switch — gone (set once at workspace creation)
- Tab state leakage between workspaces — structurally impossible
- "Remember to update X when switching" bugs — structurally impossible

## Migration Notes

- `ChatPanel` already implements per-conversation isolation via `ConversationView` — this pattern extends upward
- Each `WorkspaceView` constructor mirrors what `buildComponents()` currently does, but scoped to one workspace
- The admin subprocess model (one per workspace path) maps naturally: each `WorkspaceView` owns its admin connection
- `ProjectStateManager` remains for persistence metadata, but runtime state lives in `WorkspaceView` instances
