# Architectural Design: Server-Side Project Management

## Problem
Currently, the list of active workspaces/projects is managed entirely on the client-side via `localStorage`. This creates a fundamental disconnect when operations occur on a remote Tyde server (e.g., when an MCP agent creates a new git workbench). The remote server executes the `git worktree add`, but has no way to properly register the new workspace across all connected clients. Furthermore, any connection to a remote Tyde server starts with an empty slate rather than showing the workspaces actually active on that server.

## Desired Solution
Move project management to a server-side `projects.json` file managed by the Rust backend. Each Tyde server (local and remote) will maintain its own `projects.json`. When a client connects to a Tyde server, it fetches the active projects from that server's state. When an MCP agent creates a workbench, the server directly updates its `projects.json` and emits an event notifying all connected clients to reload the project list.

---

## 1. Rust Backend: Server-Side Storage

### `src-tauri/src/project_store.rs` (New File)
Implement a file-backed `ProjectStore` using a robust read-modify-write pattern, similar to `SessionStore`. It will be loaded at startup and stored at `~/.tyde/projects.json`.

**Data Structures:**
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRecord {
    pub id: String,
    pub name: String,
    pub workspace_path: String,
    #[serde(default)]
    pub roots: Vec<String>,
    #[serde(default)]
    pub parent_project_id: Option<String>,
    #[serde(default)]
    pub workbench_kind: Option<String>,
}

pub struct ProjectStore {
    records: HashMap<String, ProjectRecord>,
    path: PathBuf,
}
```

**Core Methods:**
- `load(path: PathBuf) -> Result<Self, String>`
- `list() -> Result<Vec<ProjectRecord>, String>`
- `add(workspace_path, name) -> Result<ProjectRecord, String>`
- `add_workbench(parent_id, worktree_path, name, kind) -> Result<ProjectRecord, String>`
- `remove(id) -> Result<(), String>`
- `get_by_workspace_path(path) -> Option<ProjectRecord>`

### App State (`src-tauri/src/lib.rs`)
Add `project_store: Arc<SyncMutex<ProjectStore>>` to `AppState` and initialize it at startup alongside `SessionStore`.

---

## 2. Tauri Command API & Routing

Expose Tauri commands that the frontend will call. Crucially, the `HostRouter` will intercept these and route them to the appropriate TydeServer based on an optional `host` parameter or the `workspace_path`.

**Commands:**
- `list_projects(host: Option<String>) -> Result<Vec<ProjectRecord>, String>`
  - If `host` is provided, invoke `list_projects` on that `TydeServerConnection`. **Crucial:** The `HostRouter` must prefix the returned `workspace_path` and `roots` with `ssh://{host}` so the frontend knows they are remote.
- `add_project(workspace_path: String, name: String) -> Result<ProjectRecord, String>`
  - Routes based on `workspace_path`. Strips `ssh://` before forwarding to a remote server.
- `remove_project(id: String, host: Option<String>)`
- `add_project_root(id: String, root: String, host: Option<String>)`
- `remove_project_root(id: String, root: String, host: Option<String>)`

---

## 3. Event System for Syncing

Instead of the ad-hoc `tyde-create-workbench` event, use a generic `project-list-changed` event.

1. **Emit:** Whenever `ProjectStore` is mutated (locally via `add_project` or remotely via MCP), the server emits `project-list-changed`.
2. **Intercept:** In `src-tauri/src/tyde_server_conn.rs`, when intercepting `project-list-changed` from a remote server, inject the `host` identifier into the payload before re-emitting to the Tauri app:
   ```rust
   let translated_event = json!({ "host": self.ssh_host() });
   self.app.emit("project-list-changed", translated_event);
   ```
3. **Consume:** The frontend listens for `project-list-changed`. If it specifies a `host`, it calls `listProjects({ host })` and merges the updated list into the UI.

---

## 4. MCP Agent Integration

### `src-tauri/src/agent_mcp_http.rs`
Update `create_workbench_internal` and `delete_workbench_internal`. Instead of emitting the old workbench events, they will:
1. Lookup the parent project by `parent_workspace_path` using `ProjectStore::get_by_workspace_path`.
2. Call `ProjectStore::add_workbench` or `remove`.
3. Emit the `project-list-changed` event.

---

## 5. Client-Side State Management

### `src/project_state.ts`
- Remove all `localStorage` persistence logic (`persist()`, `restore()`, `STORAGE_KEY`).
- Update the class to maintain its `projects` array as a purely in-memory cache.
- Expose methods `fetchLocalProjects()` and `fetchRemoteProjects(host)`.
- Reconcile incoming server data with UI state (e.g., maintaining `activeProjectId` and `conversationIds` which remain client-side UI concepts).

### Connection Auto-Load (`src/app.ts`)
- On application startup, call `listProjects({ host: undefined })` to load local projects.
- When `TydeServerConnectionState` transitions to `Connected` (in `handleTydeServerConnectionState`), automatically call `listProjects({ host: payload.host })` and add those projects to the sidebar.
- On disconnect, optionally remove those projects from the view (or mark them inactive).

### Migration Strategy
On startup, before loading server projects, `app.ts` will run a one-time migration:
```typescript
const raw = localStorage.getItem("tyde-projects");
if (raw) {
  const parsed = JSON.parse(raw);
  for (const p of parsed.projects || []) {
    if (!parseRemoteWorkspaceUri(p.workspacePath)) {
      await execute("add_project", { workspacePath: p.workspacePath, name: p.name });
    }
  }
  localStorage.removeItem("tyde-projects");
}
```
Remote projects are not migrated to the remote server during startup to avoid blocking on SSH connections. They will simply populate when the user re-connects via the Home view "Recent Workspaces" or Settings -> Hosts.

---

## Files to Modify
1. `src-tauri/src/project_store.rs` (New)
2. `src-tauri/src/lib.rs`
3. `src-tauri/src/agent_mcp_http.rs`
4. `src-tauri/src/tyde_server_conn.rs`
5. `src/bridge.ts`
6. `src/project_state.ts`
7. `src/app.ts`
8. `src/projects.ts` (Remove `tyde-create-workbench` logic)