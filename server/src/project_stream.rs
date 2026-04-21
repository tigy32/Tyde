use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use std::collections::BTreeSet;

use protocol::{
    FileEntryOp, Project, ProjectDiffScope, ProjectFileContentsPayload, ProjectFileEntry,
    ProjectFileKind, ProjectFileListPayload, ProjectGitChangeKind, ProjectGitDiffFile,
    ProjectGitDiffHunk, ProjectGitDiffLine, ProjectGitDiffLineKind, ProjectGitDiffPayload,
    ProjectGitFileStatus, ProjectGitStatusPayload, ProjectId, ProjectPath, ProjectReadDiffPayload,
    ProjectReadFilePayload, ProjectRootGitStatus, ProjectRootListing, ProjectRootPath,
};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::store::project::ProjectStore;
use crate::stream::Stream;

const PROJECT_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// A (relative_path, kind) pair used for diffing file listings between polls.
pub(crate) type RawFileEntry = (String, ProjectFileKind);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitAccessMode {
    ReadOnly,
    Mutating,
}

#[derive(Debug, Default)]
pub(crate) struct ProjectSnapshotState {
    /// Previous file entries per root, used to compute diffs.
    pub file_entries: BTreeMap<ProjectRootPath, BTreeSet<RawFileEntry>>,
    pub git_status: Option<Value>,
}

pub(crate) struct ProjectStreamSubscription {
    pub task: JoinHandle<()>,
    pub state: Arc<Mutex<ProjectSnapshotState>>,
}

pub(crate) fn spawn_project_subscription(
    project_store: Arc<Mutex<ProjectStore>>,
    project_id: ProjectId,
    stream: Stream,
    state: Arc<Mutex<ProjectSnapshotState>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let project = match load_subscription_project(&project_store, &project_id).await {
                Ok(project) => project,
                Err(error) => {
                    tracing::warn!(project_id = %project_id, error = %error, "stopping project subscription");
                    return;
                }
            };

            let current_raw = match scan_raw_entries(&project) {
                Ok(current_raw) => current_raw,
                Err(error) => {
                    tracing::warn!(
                        project_id = %project_id,
                        error = %error,
                        "stopping project subscription after file scan failure"
                    );
                    return;
                }
            };
            let git_status = match build_git_status(&project) {
                Ok(git_status) => git_status,
                Err(error) => {
                    tracing::warn!(
                        project_id = %project_id,
                        error = %error,
                        "stopping project subscription after git status failure"
                    );
                    return;
                }
            };

            let git_json = match serde_json::to_value(&git_status) {
                Ok(git_json) => git_json,
                Err(error) => {
                    tracing::warn!(
                        project_id = %project_id,
                        error = %error,
                        "stopping project subscription after git status serialization failure"
                    );
                    return;
                }
            };

            let mut snapshot = state.lock().await;

            let file_diff = diff_file_entries(&snapshot.file_entries, &current_raw);
            if !file_diff.roots.is_empty() {
                snapshot.file_entries = current_raw;
                let file_json = match serde_json::to_value(&file_diff) {
                    Ok(file_json) => file_json,
                    Err(error) => {
                        tracing::warn!(
                            project_id = %project_id,
                            error = %error,
                            "stopping project subscription after file diff serialization failure"
                        );
                        return;
                    }
                };
                drop(snapshot);
                if stream
                    .send_value(protocol::FrameKind::ProjectFileList, file_json)
                    .await
                    .is_err()
                {
                    return;
                }
                snapshot = state.lock().await;
            }

            let git_changed = snapshot.git_status.as_ref() != Some(&git_json);
            if git_changed {
                snapshot.git_status = Some(git_json.clone());
                drop(snapshot);
                if stream
                    .send_value(protocol::FrameKind::ProjectGitStatus, git_json)
                    .await
                    .is_err()
                {
                    return;
                }
            }

            sleep(PROJECT_POLL_INTERVAL).await;
        }
    })
}

async fn load_subscription_project(
    project_store: &Arc<Mutex<ProjectStore>>,
    project_id: &ProjectId,
) -> Result<Project, String> {
    let projects = project_store.lock().await.list()?;
    projects
        .into_iter()
        .find(|project| &project.id == project_id)
        .ok_or_else(|| format!("project {} disappeared while stream was active", project_id))
}

/// Default depth limit for initial file listings and polling.
/// Directories at this depth are listed but not recursed into.
const DEFAULT_FILE_LIST_DEPTH: usize = 2;

/// Scan the filesystem and return raw (path, kind) entries per root at the default depth.
pub(crate) fn scan_raw_entries(
    project: &Project,
) -> Result<BTreeMap<ProjectRootPath, BTreeSet<RawFileEntry>>, String> {
    scan_raw_entries_with_depth(project, DEFAULT_FILE_LIST_DEPTH)
}

fn scan_raw_entries_with_depth(
    project: &Project,
    max_depth: usize,
) -> Result<BTreeMap<ProjectRootPath, BTreeSet<RawFileEntry>>, String> {
    let mut result = BTreeMap::new();
    for root in &project.roots {
        let root_path = Path::new(root);
        let metadata = fs::metadata(root_path)
            .map_err(|err| format!("Failed to stat project root '{}': {err}", root))?;
        if !metadata.is_dir() {
            return Err(format!("Project root '{}' is not a directory", root));
        }
        let mut raw = Vec::new();
        collect_raw_entries(root_path, root_path, &mut raw, 0, max_depth)?;
        result.insert(ProjectRootPath(root.clone()), raw.into_iter().collect());
    }
    Ok(result)
}

/// Build an all-Add file list payload, preserving project root order.
pub(crate) fn build_file_list(project: &Project) -> Result<ProjectFileListPayload, String> {
    let mut roots = Vec::with_capacity(project.roots.len());
    for root in &project.roots {
        let root_path = Path::new(root);
        let metadata = fs::metadata(root_path)
            .map_err(|err| format!("Failed to stat project root '{}': {err}", root))?;
        if !metadata.is_dir() {
            return Err(format!("Project root '{}' is not a directory", root));
        }
        let mut raw = Vec::new();
        collect_raw_entries(root_path, root_path, &mut raw, 0, DEFAULT_FILE_LIST_DEPTH)?;
        raw.sort();
        let entries = raw
            .into_iter()
            .map(|(path, kind)| ProjectFileEntry {
                relative_path: path,
                kind,
                op: FileEntryOp::Add,
            })
            .collect();
        roots.push(ProjectRootListing {
            root: ProjectRootPath(root.clone()),
            entries,
        });
    }
    Ok(ProjectFileListPayload {
        incremental: false,
        roots,
    })
}

/// Diff previous vs current raw entries and produce a payload with Add/Remove ops.
/// Returns a payload with empty roots if nothing changed.
fn diff_file_entries(
    previous: &BTreeMap<ProjectRootPath, BTreeSet<RawFileEntry>>,
    current: &BTreeMap<ProjectRootPath, BTreeSet<RawFileEntry>>,
) -> ProjectFileListPayload {
    let mut roots = Vec::new();
    // All roots that appear in either previous or current
    let all_roots: BTreeSet<&ProjectRootPath> = previous.keys().chain(current.keys()).collect();

    for root in all_roots {
        let prev_entries = previous.get(root);
        let curr_entries = current.get(root);
        let empty = BTreeSet::new();
        let prev = prev_entries.unwrap_or(&empty);
        let curr = curr_entries.unwrap_or(&empty);

        if prev == curr {
            continue;
        }

        let mut entries = Vec::new();
        // Removed entries: in previous but not in current
        for (path, kind) in prev.difference(curr) {
            entries.push(ProjectFileEntry {
                relative_path: path.clone(),
                kind: *kind,
                op: FileEntryOp::Remove,
            });
        }
        // Added entries: in current but not in previous
        for (path, kind) in curr.difference(prev) {
            entries.push(ProjectFileEntry {
                relative_path: path.clone(),
                kind: *kind,
                op: FileEntryOp::Add,
            });
        }
        entries.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        roots.push(ProjectRootListing {
            root: root.clone(),
            entries,
        });
    }

    ProjectFileListPayload {
        incremental: false,
        roots,
    }
}

/// List entries within a specific subdirectory of a root (all Add ops).
pub(crate) fn build_dir_listing(
    project: &Project,
    root: &ProjectRootPath,
    dir_relative_path: &str,
) -> Result<ProjectFileListPayload, String> {
    validate_root(project, root)?;
    if !dir_relative_path.is_empty() {
        validate_relative_path(dir_relative_path)?;
    }

    let root_path = Path::new(&root.0);
    let dir_path = if dir_relative_path.is_empty() {
        root_path.to_path_buf()
    } else {
        root_path.join(dir_relative_path)
    };

    let metadata = fs::metadata(&dir_path)
        .map_err(|err| format!("Failed to stat directory '{}': {err}", dir_path.display()))?;
    if !metadata.is_dir() {
        return Err(format!("'{}' is not a directory", dir_path.display()));
    }

    let mut raw = Vec::new();
    collect_raw_entries(root_path, &dir_path, &mut raw, 0, DEFAULT_FILE_LIST_DEPTH)?;

    let entries: Vec<ProjectFileEntry> = raw
        .into_iter()
        .map(|(path, kind)| ProjectFileEntry {
            relative_path: path,
            kind,
            op: FileEntryOp::Add,
        })
        .collect();

    Ok(ProjectFileListPayload {
        incremental: true,
        roots: vec![ProjectRootListing {
            root: root.clone(),
            entries,
        }],
    })
}

pub(crate) fn build_git_status(project: &Project) -> Result<ProjectGitStatusPayload, String> {
    build_git_status_with_runner(project, run_git_mode)
}

fn build_git_status_with_runner<F>(
    project: &Project,
    mut run_git: F,
) -> Result<ProjectGitStatusPayload, String>
where
    F: FnMut(&str, &[&str], GitAccessMode) -> Result<String, String>,
{
    let mut roots = Vec::with_capacity(project.roots.len());

    for root in &project.roots {
        let output = run_git(
            root,
            &["status", "--porcelain=v2", "--branch"],
            GitAccessMode::ReadOnly,
        )?;
        let mut branch = None;
        let mut ahead = 0;
        let mut behind = 0;
        let mut files = BTreeMap::<String, ProjectGitFileStatus>::new();

        for line in output.lines() {
            if let Some(head) = line.strip_prefix("# branch.head ") {
                if head != "(detached)" {
                    branch = Some(head.to_owned());
                }
                continue;
            }

            if let Some(ab) = line.strip_prefix("# branch.ab ") {
                let parts: Vec<&str> = ab.split_whitespace().collect();
                assert_eq!(parts.len(), 2, "invalid branch.ab line: {}", line);
                ahead = parts[0]
                    .trim_start_matches('+')
                    .parse()
                    .unwrap_or_else(|err| panic!("invalid ahead count in '{}': {}", line, err));
                behind = parts[1]
                    .trim_start_matches('-')
                    .parse()
                    .unwrap_or_else(|err| panic!("invalid behind count in '{}': {}", line, err));
                continue;
            }

            if let Some(path) = line.strip_prefix("? ") {
                files.insert(
                    path.to_owned(),
                    ProjectGitFileStatus {
                        relative_path: path.to_owned(),
                        staged: None,
                        unstaged: None,
                        untracked: true,
                    },
                );
                continue;
            }

            if line.starts_with("1 ") || line.starts_with("2 ") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                assert!(parts.len() >= 9, "invalid porcelain status line '{}'", line);
                let xy = parts[1];
                assert_eq!(xy.len(), 2, "invalid XY status '{}'", xy);
                let path = line
                    .rsplit_once(' ')
                    .map(|(_, path)| path)
                    .unwrap_or_else(|| panic!("missing path in status line '{}'", line))
                    .split('\t')
                    .next()
                    .unwrap_or_else(|| panic!("missing path segment in status line '{}'", line))
                    .to_owned();
                files.insert(
                    path.clone(),
                    ProjectGitFileStatus {
                        relative_path: path,
                        staged: parse_change_kind(xy.as_bytes()[0] as char),
                        unstaged: parse_change_kind(xy.as_bytes()[1] as char),
                        untracked: false,
                    },
                );
            }
        }

        roots.push(ProjectRootGitStatus {
            root: ProjectRootPath(root.clone()),
            branch,
            ahead,
            behind,
            clean: files.is_empty(),
            files: files.into_values().collect(),
        });
    }

    Ok(ProjectGitStatusPayload { roots })
}

pub(crate) fn read_file(
    project: &Project,
    payload: ProjectReadFilePayload,
) -> Result<ProjectFileContentsPayload, String> {
    let path = normalize_read_path(project, payload.path)?;
    validate_project_path(project, &path)?;
    let absolute = absolute_project_path(&path)?;
    let bytes = fs::read(&absolute)
        .map_err(|err| format!("Failed to read file '{}': {err}", absolute.display()))?;
    match String::from_utf8(bytes) {
        Ok(contents) => Ok(ProjectFileContentsPayload {
            path,
            contents: Some(contents),
            is_binary: false,
        }),
        Err(_) => Ok(ProjectFileContentsPayload {
            path,
            contents: None,
            is_binary: true,
        }),
    }
}

pub(crate) fn read_diff(
    project: &Project,
    payload: ProjectReadDiffPayload,
) -> Result<ProjectGitDiffPayload, String> {
    read_diff_with_runner(project, payload, run_git_mode)
}

fn read_diff_with_runner<F>(
    project: &Project,
    payload: ProjectReadDiffPayload,
    mut run_git: F,
) -> Result<ProjectGitDiffPayload, String>
where
    F: FnMut(&str, &[&str], GitAccessMode) -> Result<String, String>,
{
    validate_root(project, &payload.root)?;
    if let Some(path) = &payload.path {
        validate_relative_path(path)?;
    }

    let mut args = vec!["diff"];
    if matches!(payload.scope, ProjectDiffScope::Staged) {
        args.push("--cached");
    }
    if let Some(path) = &payload.path {
        args.push("--");
        args.push(path);
    }

    let raw = run_git(&payload.root.0, &args, GitAccessMode::ReadOnly)?;
    Ok(ProjectGitDiffPayload {
        root: payload.root,
        scope: payload.scope,
        path: payload.path,
        files: parse_git_diff(&raw),
    })
}

pub(crate) fn stage_file(project: &Project, path: &ProjectPath) -> Result<(), String> {
    validate_project_path(project, path)?;
    run_git_mode(
        &path.root.0,
        &["add", "--", &path.relative_path],
        GitAccessMode::Mutating,
    )?;
    Ok(())
}

pub(crate) fn stage_hunk(
    project: &Project,
    path: &ProjectPath,
    hunk_id: &str,
) -> Result<(), String> {
    validate_project_path(project, path)?;
    if hunk_id.trim().is_empty() {
        return Err("project_stage_hunk hunk_id must not be empty".to_owned());
    }

    let raw = run_git_mode(
        &path.root.0,
        &["diff", "--", &path.relative_path],
        GitAccessMode::ReadOnly,
    )?;
    let parsed = parse_raw_git_diff(&raw);
    let Some(file) = parsed
        .iter()
        .find(|file| file.relative_path == path.relative_path)
    else {
        return Err(format!(
            "No unstaged diff exists for '{}'",
            path.relative_path
        ));
    };

    let Some((_, hunk)) = file
        .hunks
        .iter()
        .enumerate()
        .find(|(index, _)| build_hunk_id(&file.relative_path, *index) == hunk_id)
    else {
        return Err(format!(
            "Unknown hunk id '{}' for '{}'",
            hunk_id, path.relative_path
        ));
    };

    let mut patch = String::new();
    for line in &file.header_lines {
        patch.push_str(line);
        patch.push('\n');
    }
    patch.push_str(&hunk.header);
    patch.push('\n');
    for line in &hunk.lines {
        patch.push_str(line);
        patch.push('\n');
    }

    run_git_with_stdin_mode(
        &path.root.0,
        &["apply", "--cached", "--recount", "--whitespace=nowarn", "-"],
        &patch,
        GitAccessMode::Mutating,
    )?;
    Ok(())
}

pub(crate) async fn sync_snapshot_state(
    state: &Arc<Mutex<ProjectSnapshotState>>,
    raw_entries: &BTreeMap<ProjectRootPath, BTreeSet<RawFileEntry>>,
    git_status: &ProjectGitStatusPayload,
) {
    let mut snapshot = state.lock().await;
    snapshot.file_entries = raw_entries.clone();
    snapshot.git_status = match serde_json::to_value(git_status) {
        Ok(git_status) => Some(git_status),
        Err(error) => {
            tracing::warn!(
                error = %error,
                "failed to serialize project git status snapshot"
            );
            None
        }
    };
}

fn collect_raw_entries(
    root: &Path,
    current: &Path,
    out: &mut Vec<RawFileEntry>,
    depth: usize,
    max_depth: usize,
) -> Result<(), String> {
    let mut entries = fs::read_dir(current)
        .map_err(|err| format!("Failed to read directory '{}': {err}", current.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("Failed to iterate directory '{}': {err}", current.display()))?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let file_name = entry.file_name();
        if file_name == ".git" {
            continue;
        }

        let metadata = fs::symlink_metadata(&path)
            .map_err(|err| format!("Failed to stat path '{}': {err}", path.display()))?;
        let relative_path = path
            .strip_prefix(root)
            .map_err(|err| {
                format!(
                    "failed to strip root prefix from '{}': {}",
                    path.display(),
                    err
                )
            })?
            .to_string_lossy()
            .replace('\\', "/");

        let kind = if metadata.file_type().is_symlink() {
            ProjectFileKind::Symlink
        } else if metadata.is_dir() {
            ProjectFileKind::Directory
        } else {
            ProjectFileKind::File
        };

        out.push((relative_path, kind));

        // Recurse into directories only if within depth limit
        if metadata.is_dir() && depth < max_depth {
            collect_raw_entries(root, &path, out, depth + 1, max_depth)?;
        }
    }

    Ok(())
}

fn validate_root(project: &Project, root: &ProjectRootPath) -> Result<(), String> {
    if project.roots.iter().any(|candidate| candidate == &root.0) {
        return Ok(());
    }
    Err(format!(
        "Root '{}' does not belong to project {}",
        root, project.id
    ))
}

fn validate_project_path(project: &Project, path: &ProjectPath) -> Result<(), String> {
    validate_root(project, &path.root)?;
    validate_relative_path(&path.relative_path)
}

fn normalize_read_path(project: &Project, path: ProjectPath) -> Result<ProjectPath, String> {
    let normalized_relative_path = normalize_file_reference(&path.relative_path)?;

    if let Some(path) = project_path_from_absolute(project, &normalized_relative_path) {
        return Ok(path);
    }

    Ok(ProjectPath {
        root: path.root,
        relative_path: normalized_relative_path,
    })
}

fn normalize_file_reference(path: &str) -> Result<String, String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("relative path must not be empty".to_owned());
    }

    let decoded = percent_decode_path(trimmed).unwrap_or_else(|| trimmed.to_owned());
    let without_scheme = decoded.strip_prefix("file://").unwrap_or(decoded.as_str());
    let without_fragment = without_scheme.split('#').next().unwrap_or(without_scheme);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    let without_line_suffix = strip_trailing_line_suffix(without_query);
    let normalized = without_line_suffix.trim_start_matches("./");

    if normalized.trim().is_empty() {
        return Err("relative path must not be empty".to_owned());
    }

    Ok(normalized.to_owned())
}

fn strip_trailing_line_suffix(path: &str) -> &str {
    let mut candidate = path;
    for _ in 0..2 {
        let Some((prefix, suffix)) = candidate.rsplit_once(':') else {
            break;
        };
        if suffix.chars().all(|ch| ch.is_ascii_digit()) {
            candidate = prefix;
        } else {
            break;
        }
    }
    candidate
}

fn project_path_from_absolute(project: &Project, absolute_path: &str) -> Option<ProjectPath> {
    let absolute = Path::new(absolute_path);
    if !absolute.is_absolute() {
        return None;
    }

    for root in &project.roots {
        let Ok(relative) = absolute.strip_prefix(root) else {
            continue;
        };
        let relative_path = relative.to_string_lossy().replace('\\', "/");
        if relative_path.is_empty() {
            return None;
        }
        return Some(ProjectPath {
            root: ProjectRootPath(root.clone()),
            relative_path,
        });
    }

    None
}

fn percent_decode_path(value: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut chars = value.as_bytes().iter().copied();
    while let Some(byte) = chars.next() {
        match byte {
            b'%' => {
                let high = chars.next()?;
                let low = chars.next()?;
                let decoded = (decode_hex_nibble(high)? << 4) | decode_hex_nibble(low)?;
                bytes.push(decoded);
            }
            _ => bytes.push(byte),
        }
    }
    String::from_utf8(bytes).ok()
}

fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn validate_relative_path(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("relative path must not be empty".to_owned());
    }

    let relative = Path::new(path);
    if !relative.is_relative() {
        return Err(format!("project relative path must be relative: {}", path));
    }

    for component in relative.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!(
                    "project relative path must not contain '..': {}",
                    path
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("project relative path must be relative: {}", path));
            }
        }
    }

    Ok(())
}

fn absolute_project_path(path: &ProjectPath) -> Result<PathBuf, String> {
    validate_relative_path(&path.relative_path)?;
    Ok(Path::new(&path.root.0).join(&path.relative_path))
}

fn run_git_mode(root: &str, args: &[&str], access_mode: GitAccessMode) -> Result<String, String> {
    run_git_mode_with_binary("git", root, args, access_mode)
}

fn run_git_mode_with_binary(
    git_binary: impl AsRef<std::ffi::OsStr>,
    root: &str,
    args: &[&str],
    access_mode: GitAccessMode,
) -> Result<String, String> {
    let output = git_command(git_binary, root, args, access_mode)
        .output()
        .map_err(|err| format!("Failed to run git in '{}': {err}", root))?;
    if !output.status.success() {
        return Err(format!(
            "git {:?} failed in '{}': {}",
            args,
            root,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|err| format!("git output was not valid UTF-8 in '{}': {err}", root))
}

fn run_git_with_stdin_mode(
    root: &str,
    args: &[&str],
    stdin: &str,
    access_mode: GitAccessMode,
) -> Result<String, String> {
    run_git_with_stdin_mode_with_binary("git", root, args, stdin, access_mode)
}

fn run_git_with_stdin_mode_with_binary(
    git_binary: impl AsRef<std::ffi::OsStr>,
    root: &str,
    args: &[&str],
    stdin: &str,
    access_mode: GitAccessMode,
) -> Result<String, String> {
    let mut child = git_command(git_binary, root, args, access_mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("Failed to start git in '{}': {err}", root))?;

    use std::io::Write;
    let mut stdin_pipe = child
        .stdin
        .take()
        .unwrap_or_else(|| panic!("git stdin pipe missing for args {:?}", args));
    stdin_pipe
        .write_all(stdin.as_bytes())
        .map_err(|err| format!("Failed to write git stdin in '{}': {err}", root))?;
    drop(stdin_pipe);

    let output = child
        .wait_with_output()
        .map_err(|err| format!("Failed to wait for git in '{}': {err}", root))?;
    if !output.status.success() {
        return Err(format!(
            "git {:?} failed in '{}': {}",
            args,
            root,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|err| format!("git output was not valid UTF-8 in '{}': {err}", root))
}

fn git_command(
    git_binary: impl AsRef<std::ffi::OsStr>,
    root: &str,
    args: &[&str],
    access_mode: GitAccessMode,
) -> Command {
    let mut command = Command::new(git_binary);
    if matches!(access_mode, GitAccessMode::ReadOnly) {
        command.arg("--no-optional-locks");
    }
    command.arg("-C").arg(root).args(args);
    command
}

fn parse_change_kind(status: char) -> Option<ProjectGitChangeKind> {
    match status {
        '.' | ' ' => None,
        'A' => Some(ProjectGitChangeKind::Added),
        'M' => Some(ProjectGitChangeKind::Modified),
        'D' => Some(ProjectGitChangeKind::Deleted),
        'R' => Some(ProjectGitChangeKind::Renamed),
        'C' => Some(ProjectGitChangeKind::Copied),
        'T' => Some(ProjectGitChangeKind::TypeChanged),
        other => panic!("unsupported git change kind '{}'", other),
    }
}

#[derive(Debug, Clone)]
struct ParsedGitDiffFile {
    relative_path: String,
    header_lines: Vec<String>,
    hunks: Vec<ParsedGitDiffHunk>,
}

#[derive(Debug, Clone)]
struct ParsedGitDiffHunk {
    header: String,
    lines: Vec<String>,
}

fn parse_git_diff(raw: &str) -> Vec<ProjectGitDiffFile> {
    parse_raw_git_diff(raw)
        .into_iter()
        .map(|file| {
            let relative_path = file.relative_path.clone();
            ProjectGitDiffFile {
                relative_path: relative_path.clone(),
                hunks: file
                    .hunks
                    .into_iter()
                    .enumerate()
                    .map(|(index, hunk)| ProjectGitDiffHunk {
                        hunk_id: build_hunk_id(&relative_path, index),
                        header: hunk.header,
                        lines: hunk
                            .lines
                            .into_iter()
                            .map(|line| ProjectGitDiffLine {
                                kind: classify_diff_line(&line),
                                text: line,
                            })
                            .collect(),
                    })
                    .collect(),
            }
        })
        .collect()
}

fn parse_raw_git_diff(raw: &str) -> Vec<ParsedGitDiffFile> {
    let mut files = Vec::new();
    let mut current_file: Option<ParsedGitDiffFile> = None;
    let mut current_hunk: Option<ParsedGitDiffHunk> = None;

    for line in raw.lines() {
        if let Some(diff_line) = line.strip_prefix("diff --git ") {
            if let Some(hunk) = current_hunk.take() {
                current_file
                    .as_mut()
                    .unwrap_or_else(|| panic!("hunk appeared before file in git diff"))
                    .hunks
                    .push(hunk);
            }
            if let Some(file) = current_file.take() {
                files.push(file);
            }

            let parts: Vec<&str> = diff_line.split_whitespace().collect();
            assert_eq!(parts.len(), 2, "invalid diff header '{}'", line);
            current_file = Some(ParsedGitDiffFile {
                relative_path: parse_diff_path(parts[0], parts[1]),
                header_lines: vec![line.to_owned()],
                hunks: Vec::new(),
            });
            continue;
        }

        if line.starts_with("@@") {
            if let Some(hunk) = current_hunk.take() {
                current_file
                    .as_mut()
                    .unwrap_or_else(|| panic!("hunk appeared before file in git diff"))
                    .hunks
                    .push(hunk);
            }
            current_hunk = Some(ParsedGitDiffHunk {
                header: line.to_owned(),
                lines: Vec::new(),
            });
            continue;
        }

        if let Some(hunk) = current_hunk.as_mut() {
            hunk.lines.push(line.to_owned());
            continue;
        }

        if let Some(file) = current_file.as_mut() {
            file.header_lines.push(line.to_owned());
        }
    }

    if let Some(hunk) = current_hunk.take() {
        current_file
            .as_mut()
            .unwrap_or_else(|| panic!("trailing hunk appeared before file in git diff"))
            .hunks
            .push(hunk);
    }
    if let Some(file) = current_file.take() {
        files.push(file);
    }

    files
}

fn parse_diff_path(a_path: &str, b_path: &str) -> String {
    if let Some(path) = b_path.strip_prefix("b/")
        && path != "dev/null"
    {
        return path.to_owned();
    }
    a_path.strip_prefix("a/").unwrap_or(a_path).to_owned()
}

fn build_hunk_id(relative_path: &str, index: usize) -> String {
    format!("{}::{}", relative_path, index)
}

fn classify_diff_line(line: &str) -> ProjectGitDiffLineKind {
    match line.chars().next() {
        Some('+') => ProjectGitDiffLineKind::Added,
        Some('-') => ProjectGitDiffLineKind::Removed,
        _ => ProjectGitDiffLineKind::Context,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use protocol::{ProjectDiffScope, ProjectId};
    use std::fs;
    use std::path::{Path, PathBuf};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn test_project(root: &str) -> Project {
        Project {
            id: ProjectId("project-1".to_owned()),
            name: "Project".to_owned(),
            roots: vec![root.to_owned()],
            sort_order: 0,
        }
    }

    #[test]
    fn build_git_status_uses_read_only_git_access() {
        let project = test_project("/repo");
        let mut calls = Vec::new();

        let status = build_git_status_with_runner(&project, |root, args, access_mode| {
            calls.push((
                root.to_owned(),
                args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
                access_mode,
            ));
            Ok("# branch.oid abc123\n# branch.head main\n".to_owned())
        })
        .expect("build_git_status should succeed");

        assert_eq!(
            calls,
            vec![(
                "/repo".to_owned(),
                vec![
                    "status".to_owned(),
                    "--porcelain=v2".to_owned(),
                    "--branch".to_owned(),
                ],
                GitAccessMode::ReadOnly,
            )]
        );
        assert_eq!(status.roots.len(), 1);
        assert_eq!(status.roots[0].branch.as_deref(), Some("main"));
        assert!(status.roots[0].clean);
    }

    #[test]
    fn read_diff_uses_read_only_git_access() {
        let project = test_project("/repo");
        let mut calls = Vec::new();

        let diff = read_diff_with_runner(
            &project,
            ProjectReadDiffPayload {
                root: ProjectRootPath("/repo".to_owned()),
                scope: ProjectDiffScope::Unstaged,
                path: Some("src/lib.rs".to_owned()),
            },
            |root, args, access_mode| {
                calls.push((
                    root.to_owned(),
                    args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
                    access_mode,
                ));
                Ok(String::new())
            },
        )
        .expect("read_diff should succeed");

        assert_eq!(
            calls,
            vec![(
                "/repo".to_owned(),
                vec!["diff".to_owned(), "--".to_owned(), "src/lib.rs".to_owned(),],
                GitAccessMode::ReadOnly,
            )]
        );
        assert_eq!(diff.root.0, "/repo");
        assert_eq!(diff.path.as_deref(), Some("src/lib.rs"));
    }

    #[cfg(unix)]
    struct FakeGitBinary {
        dir: PathBuf,
        binary: PathBuf,
        log_path: PathBuf,
    }

    #[cfg(unix)]
    impl FakeGitBinary {
        fn new(stdout: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("tyde-project-stream-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).expect("create fake git dir");

            let binary = dir.join("git");
            let stdout_path = dir.join("stdout.txt");
            let log_path = dir.join("args.log");

            fs::write(&stdout_path, stdout).expect("write fake git stdout");
            fs::write(
                &binary,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncat '{}'\n",
                    log_path.display(),
                    stdout_path.display()
                ),
            )
            .expect("write fake git script");

            let mut permissions = fs::metadata(&binary)
                .expect("stat fake git script")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&binary, permissions).expect("chmod fake git script");

            Self {
                dir,
                binary,
                log_path,
            }
        }

        fn binary_path(&self) -> &Path {
            &self.binary
        }

        fn logged_args(&self) -> Vec<String> {
            fs::read_to_string(&self.log_path)
                .expect("read fake git log")
                .lines()
                .map(|line| line.to_owned())
                .collect()
        }
    }

    #[cfg(unix)]
    impl Drop for FakeGitBinary {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[cfg(unix)]
    #[test]
    fn read_only_git_commands_disable_optional_locks() {
        let fake_git = FakeGitBinary::new("# branch.oid abc123\n# branch.head main\n");

        let output = run_git_mode_with_binary(
            fake_git.binary_path(),
            "/repo",
            &["status", "--porcelain=v2", "--branch"],
            GitAccessMode::ReadOnly,
        )
        .expect("read-only git command should succeed");

        assert!(output.contains("# branch.head main"));
        assert_eq!(
            fake_git.logged_args(),
            vec![
                "--no-optional-locks".to_owned(),
                "-C".to_owned(),
                "/repo".to_owned(),
                "status".to_owned(),
                "--porcelain=v2".to_owned(),
                "--branch".to_owned(),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn mutating_git_commands_do_not_disable_locks() {
        let fake_git = FakeGitBinary::new("");

        run_git_mode_with_binary(
            fake_git.binary_path(),
            "/repo",
            &["add", "--", "src/lib.rs"],
            GitAccessMode::Mutating,
        )
        .expect("mutating git command should succeed");

        assert_eq!(
            fake_git.logged_args(),
            vec![
                "-C".to_owned(),
                "/repo".to_owned(),
                "add".to_owned(),
                "--".to_owned(),
                "src/lib.rs".to_owned(),
            ]
        );
    }
}
