# Project Stream

This document specifies the project stream used for file browsing, file reads,
git status, and git diffs within a project.

It builds on:

- `01-philosophy.md` for architectural constraints
- `02-protocol.md` for stream and event rules
- `06-projects.md` for project identity and storage

---

## 1. Goals

We want a dedicated **project stream** that lets the frontend render a project
browser without reconstructing filesystem or git semantics on its own.

The stream must provide enough typed state to build:

- a multi-root file browser
- file open / read
- git status per root
- staged and unstaged diff views
- stage-file and stage-hunk actions

This is explicitly server-owned behavior:

- the frontend does not run `git status`, `git diff`, `ls`, or filesystem
  watchers on its own
- the server emits typed project state and typed change events
- local and remote projects use the same protocol and the same UI model

---

## 2. Stream URL

Project streams use this path:

```text
/project/<project_id>
```

Example:

```text
/project/550e8400-e29b-41d4-a716-446655440000
```

Rules:

- The stream is client-initiated.
- `project_id` is the project identity from `ProjectId`.
- There is one project stream per project per connection.
- The stream remains open for the lifetime of the connection or until the
  client stops using it.
- All commands and state updates for that project flow on this stream.

Why no per-subscriber instance ID:

- the stream is not a server-created runtime entity like an agent
- project identity already exists and is stable
- the stream is a connection-local subscription to that one project

The existing sequence number rules still apply. Each side maintains its own
monotonic sequence counter for `/project/<project_id>`.

---

## 3. Core Model

Projects have multiple roots, and each root is expected to be a git root.

The UI must show all roots as top-level directories. Because roots may have the
same basename, the browser cannot identify a root by display name alone.

So every file- and git-related payload must carry an explicit root selector.

### 3.1 Root identity

The authoritative root identity is the exact root path already stored on the
project:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectRootPath(pub String);
```

This is explicit data, not inferred UI state.

There is no separate root name. A root is just its path.

### 3.2 File identity

Files inside a project are addressed by root plus relative path:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectPath {
    pub root: ProjectRootPath,
    pub relative_path: String,
}
```

This avoids ambiguity across multiple roots and avoids any frontend-side path
guessing.

### 3.3 Git scope

Git state is per root, not per whole project. A project may span multiple
repositories with different branches and different dirty states.

So `project_git_status` is a list of per-root git snapshots, not one flattened
global status.

---

## 4. Event Model

The project stream is event-driven like the rest of Tyde.

The client sends input events on `/project/<project_id>`.
The server emits output events on `/project/<project_id>`.

There are no request IDs and no request/response pairing.

Instead:

- read commands carry enough context that the resulting output event can be
  understood on its own
- refresh and live change notifications use the same output event shapes
- the UI renders the latest state it has received

This means the server is free to emit:

- `project_file_list` after `project_refresh`
- `project_git_status` after `project_refresh`
- another `project_git_status` later because the repo changed
- another `project_file_list` later because filesystem contents changed

The frontend reacts to state. It does not assume one output per input.

---

## 5. Input Events

These are sent on `/project/<project_id>`.

### 5.1 `project_refresh`

Forces the server to rescan the project and emit fresh current-state snapshots.

```rust
pub struct ProjectRefreshPayload {}
```

Expected outputs:

- `project_file_list`
- `project_git_status`

The server may emit them in that order consistently for predictability.

### 5.2 `project_read_file`

Requests file contents for one file.

```rust
pub struct ProjectReadFilePayload {
    pub path: ProjectPath,
}
```

Expected output:

- `project_file_contents`

### 5.3 `project_read_diff`

Requests a git diff for one root and scope.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectDiffScope {
    Unstaged,
    Staged,
}

pub struct ProjectReadDiffPayload {
    pub root: ProjectRootPath,
    pub scope: ProjectDiffScope,
    pub path: Option<String>,
}
```

Rules:

- `path: None` means read the full diff for that root+scope.
- `path: Some(relative_path)` means read the diff only for that file.

Expected output:

- `project_git_diff`

### 5.4 `project_stage_file`

Stages one file within one root.

```rust
pub struct ProjectStageFilePayload {
    pub path: ProjectPath,
}
```

Expected outputs:

- `project_git_status`
- optionally `project_git_diff` if the server chooses to refresh the relevant
  diff view

### 5.5 `project_stage_hunk`

Stages one hunk within one file.

The frontend must not construct patch text itself. Hunks must be server-defined
and server-identified.

```rust
pub struct ProjectStageHunkPayload {
    pub path: ProjectPath,
    pub hunk_id: String,
}
```

`hunk_id` comes from `project_git_diff`.

Expected outputs:

- `project_git_status`
- `project_git_diff`

---

## 6. Output Events

These are emitted on `/project/<project_id>`.

### 6.1 `project_file_list`

Full snapshot of the file browser state for all roots.

```rust
pub struct ProjectFileListPayload {
    pub roots: Vec<ProjectRootListing>,
}

pub struct ProjectRootListing {
    pub root: ProjectRootPath,
    pub entries: Vec<ProjectFileEntry>,
}

pub struct ProjectFileEntry {
    pub relative_path: String,
    pub name: String,
    pub kind: ProjectFileKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectFileKind {
    File,
    Directory,
    Symlink,
}
```

Design choice:

- emit full per-root listings, not incremental tree patches

Why:

- simpler semantics
- one call path
- easier replay and easier correctness
- no hidden cache invalidation logic in the UI

If performance later proves this too expensive, we can optimize with profiling
data. Not before.

### 6.2 `project_git_status`

Full git snapshot for all roots.

```rust
pub struct ProjectGitStatusPayload {
    pub roots: Vec<ProjectRootGitStatus>,
}

pub struct ProjectRootGitStatus {
    pub root: ProjectRootPath,
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub clean: bool,
    pub files: Vec<ProjectGitFileStatus>,
}

pub struct ProjectGitFileStatus {
    pub relative_path: String,
    pub staged: Option<ProjectGitChangeKind>,
    pub unstaged: Option<ProjectGitChangeKind>,
    pub untracked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectGitChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
}
```

This gives the UI enough typed data to render:

- branch name
- ahead/behind counters
- staged vs unstaged state
- untracked files

without parsing raw git output.

### 6.3 `project_file_contents`

Contents for one file read request.

```rust
pub struct ProjectFileContentsPayload {
    pub path: ProjectPath,
    pub version: u64,
    pub encoding: ProjectFileEncoding,
    pub contents: Option<String>,
    pub is_binary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectFileEncoding {
    Utf8,
}
```

Rules:

- `path` is echoed back so the event is self-describing.
- `version` is a monotonic per-file snapshot version assigned by the server.
- `contents: None` with `is_binary: true` means the file is binary and not
  returned as text.

### 6.4 `project_git_diff`

Structured diff payload for one root+scope, optionally narrowed to one file.

```rust
pub struct ProjectGitDiffPayload {
    pub root: ProjectRootPath,
    pub scope: ProjectDiffScope,
    pub path: Option<String>,
    pub files: Vec<ProjectGitDiffFile>,
}

pub struct ProjectGitDiffFile {
    pub relative_path: String,
    pub hunks: Vec<ProjectGitDiffHunk>,
}

pub struct ProjectGitDiffHunk {
    pub hunk_id: String,
    pub header: String,
    pub lines: Vec<ProjectGitDiffLine>,
}

pub struct ProjectGitDiffLine {
    pub kind: ProjectGitDiffLineKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectGitDiffLineKind {
    Context,
    Added,
    Removed,
}
```

Important:

- the UI renders this diff directly
- the UI does not parse raw patch text to discover hunk boundaries
- `hunk_id` is the stable server-issued handle used by `project_stage_hunk`

---

## 7. Refresh and Live Updates

The project stream is both:

- a command stream for reads and mutations
- a subscription stream for live project state

### 7.1 Initial usage

Recommended client flow:

1. open `/project/<project_id>`
2. send `project_refresh`
3. render the resulting `project_file_list` and `project_git_status`
4. send `project_read_file` or `project_read_diff` as the user navigates

### 7.2 Live changes

After refresh, the server watches the project's roots for:

- filesystem changes
- git status changes

When relevant state changes, it emits fresh snapshots on the same stream:

- `project_file_list`
- `project_git_status`

This uses the same event model as explicit refresh. No separate "changed"
events are needed initially.

### 7.3 Why snapshots instead of incremental patches

Because the philosophy document is right:

- hidden caches are a smell
- invalid states should be unrepresentable
- the UI should render what the server says, not repair drift

Full snapshots are simpler and more correct. We can optimize later if the
measurements justify it.

---

## 8. Multi-Root Browser Semantics

Projects have multiple roots. The browser must show each root as a top-level
directory.

Example:

```text
My Project
├── repo-a/
│   └── src/...
└── repo-b/
    └── app/...
```

But because `repo-a` and `repo-b` might not be unique basenames across
projects, the protocol identity is still `ProjectRootPath`, not display label.

So the UI rules are:

- render one top-level section per root
- use the root path itself as the label unless the UI chooses a purely local
  display transform
- use `root` for all actions and keys
- use `ProjectPath { root, relative_path }` for file operations

The frontend never guesses which repository a file belongs to.

---

## 9. Failure Model

If the project does not exist, the root does not belong to the project, the
file path is invalid, or the hunk ID is unknown, the server should fail loudly.

That likely means one of:

- panic for protocol violation / impossible state
- explicit typed error event, if we later decide project streams need one

What we must not do:

- silently ignore bad paths
- guess another root
- return partial state and pretend success

---

## 10. Non-Goals

Not part of the first version:

- file writes / save
- rename / move / delete file operations
- unstage operations
- commit / push operations
- blame, history, or log browsing
- incremental tree patches
- binary diff rendering

Those can come later, but this stream should first provide a clean typed model
for browse, read, status, diff, and staging.
