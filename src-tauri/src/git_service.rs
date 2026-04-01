use serde::Serialize;
use tokio::process::Command;

use crate::remote::{parse_remote_path, run_ssh_command};

pub async fn discover_git_repos(workspace_dir: &str) -> Result<Vec<String>, String> {
    // Check if the workspace dir itself is a git repo
    let check = Command::new("git")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["rev-parse", "--git-dir"])
        .current_dir(workspace_dir)
        .output()
        .await
        .map_err(|e| format!("{e:?}"))?;

    if check.status.success() {
        return Ok(vec![workspace_dir.to_string()]);
    }

    // Scan immediate children for .git directories
    let mut repos = Vec::new();
    let mut entries = tokio::fs::read_dir(workspace_dir)
        .await
        .map_err(|e| format!("Failed to read directory: {e}"))?;

    while let Some(entry) = entries.next_entry().await.map_err(|e| format!("{e}"))? {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.join(".git").exists() {
            if let Some(s) = path.to_str() {
                repos.push(s.to_string());
            }
        }
    }

    repos.sort();
    Ok(repos)
}

#[derive(Serialize, Clone)]
pub struct GitFileStatus {
    pub path: String,
    pub status: FileStatus,
    pub staged: bool,
}

#[derive(Serialize, Clone)]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

async fn run_git(working_dir: &str, args: &[&str]) -> Result<String, String> {
    run_git_with_statuses(working_dir, args, &[], false).await
}

fn status_allowed(status: &std::process::ExitStatus, allowed_statuses: &[i32]) -> bool {
    status.success()
        || status
            .code()
            .is_some_and(|code| allowed_statuses.contains(&code))
}

async fn run_git_readonly(working_dir: &str, args: &[&str]) -> Result<String, String> {
    run_git_with_statuses(working_dir, args, &[], true).await
}

async fn run_git_readonly_allow_statuses(
    working_dir: &str,
    args: &[&str],
    allowed_statuses: &[i32],
) -> Result<String, String> {
    run_git_with_statuses(working_dir, args, allowed_statuses, true).await
}

async fn run_git_with_statuses(
    working_dir: &str,
    args: &[&str],
    allowed_statuses: &[i32],
    disable_optional_locks: bool,
) -> Result<String, String> {
    if let Some(remote) = parse_remote_path(working_dir) {
        let mut ssh_args = Vec::new();
        if disable_optional_locks {
            ssh_args.push("env".to_string());
            ssh_args.push("GIT_OPTIONAL_LOCKS=0".to_string());
        }
        ssh_args.push("git".to_string());
        ssh_args.push("-C".to_string());
        ssh_args.push(remote.path);
        ssh_args.extend(args.iter().map(|arg| arg.to_string()));

        let output = run_ssh_command(&remote.host, &ssh_args).await?;
        if !status_allowed(&output.status, allowed_statuses) {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("ssh git {}: {}", args.join(" "), stderr.trim()));
        }

        return String::from_utf8(output.stdout).map_err(|e| format!("{e:?}"));
    }

    let mut command = Command::new("git");
    command.args(args).current_dir(working_dir);
    if disable_optional_locks {
        // Read-only calls should avoid creating optional repo/index locks.
        command.env("GIT_OPTIONAL_LOCKS", "0");
    }

    let output = command.output().await.map_err(|e| format!("{e:?}"))?;

    if !status_allowed(&output.status, allowed_statuses) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {}: {stderr}", args.join(" ")));
    }

    String::from_utf8(output.stdout).map_err(|e| format!("{e:?}"))
}

fn is_missing_blob_error(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("does not exist in 'head'")
        || lower.contains("exists on disk, but not in 'head'")
        || lower.contains("exists on disk, but not in the index")
        || lower.contains("pathspec")
}

async fn git_show_blob_or_empty(working_dir: &str, object_spec: &str) -> Result<String, String> {
    if let Some(remote) = parse_remote_path(working_dir) {
        let ssh_args = vec![
            "env".to_string(),
            "GIT_OPTIONAL_LOCKS=0".to_string(),
            "git".to_string(),
            "-C".to_string(),
            remote.path,
            "show".to_string(),
            object_spec.to_string(),
        ];

        let output = run_ssh_command(&remote.host, &ssh_args).await?;
        if output.status.success() {
            return String::from_utf8(output.stdout).map_err(|e| format!("{e:?}"));
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_missing_blob_error(&stderr) {
            return Ok(String::new());
        }

        return Err(format!("ssh git show {}: {}", object_spec, stderr.trim()));
    }

    let output = Command::new("git")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["show", object_spec])
        .current_dir(working_dir)
        .output()
        .await
        .map_err(|e| format!("{e:?}"))?;

    if output.status.success() {
        return String::from_utf8(output.stdout).map_err(|e| format!("{e:?}"));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_missing_blob_error(&stderr) {
        return Ok(String::new());
    }

    Err(format!("git show {}: {}", object_spec, stderr.trim()))
}

fn parse_status_char(c: char) -> Option<FileStatus> {
    match c {
        'M' => Some(FileStatus::Modified),
        'A' => Some(FileStatus::Added),
        'D' => Some(FileStatus::Deleted),
        'R' => Some(FileStatus::Renamed),
        'U' => Some(FileStatus::Conflicted),
        _ => None,
    }
}

pub async fn git_status(working_dir: &str) -> Result<Vec<GitFileStatus>, String> {
    let output = run_git_readonly(
        working_dir,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )
    .await?;
    let mut entries = Vec::new();

    for line in output.lines() {
        if line.len() < 3 {
            continue;
        }

        let bytes = line.as_bytes();
        let index_char = bytes[0] as char;
        let worktree_char = bytes[1] as char;
        let path = line[3..].to_string();

        if index_char == '?' && worktree_char == '?' {
            entries.push(GitFileStatus {
                path,
                status: FileStatus::Untracked,
                staged: false,
            });
            continue;
        }

        if let Some(status) = parse_status_char(index_char) {
            entries.push(GitFileStatus {
                path: path.clone(),
                status,
                staged: true,
            });
        }

        if let Some(status) = parse_status_char(worktree_char) {
            entries.push(GitFileStatus {
                path,
                status,
                staged: false,
            });
        }
    }

    Ok(entries)
}

pub async fn git_stage(working_dir: &str, paths: &[String]) -> Result<(), String> {
    let path_refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
    let mut args = vec!["add", "--"];
    args.extend(path_refs);
    run_git(working_dir, &args).await?;
    Ok(())
}

pub async fn git_unstage(working_dir: &str, paths: &[String]) -> Result<(), String> {
    let path_refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
    let mut args = vec!["restore", "--staged", "--"];
    args.extend(path_refs.iter());
    match run_git(working_dir, &args).await {
        Ok(_) => Ok(()),
        Err(err) if err.contains("bad default revision") || err.contains("unknown revision") => {
            // Empty repos have no HEAD, so restore --staged fails; use rm --cached instead
            let mut rm_args = vec!["rm", "--cached", "--"];
            rm_args.extend(path_refs.iter());
            run_git(working_dir, &rm_args).await?;
            Ok(())
        }
        Err(err) => Err(err),
    }
}

pub async fn git_commit(working_dir: &str, message: &str) -> Result<String, String> {
    run_git(working_dir, &["commit", "-m", message]).await?;
    let hash = run_git_readonly(working_dir, &["rev-parse", "HEAD"]).await?;
    Ok(hash.trim().to_string())
}

pub async fn git_diff(working_dir: &str, path: &str, staged: bool) -> Result<String, String> {
    if staged {
        return run_git_readonly(working_dir, &["diff", "--staged", "--", path]).await;
    }

    let diff = run_git_readonly(working_dir, &["diff", "--", path]).await?;
    if !diff.trim().is_empty() {
        return Ok(diff);
    }

    // `git diff` intentionally omits untracked files. Detect this case and
    // synthesize a patch from /dev/null so the IDE can show file contents.
    let untracked = run_git_readonly(
        working_dir,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            path,
        ],
    )
    .await?;
    if untracked.is_empty() {
        return Ok(diff);
    }

    run_git_readonly_allow_statuses(
        working_dir,
        &["diff", "--no-index", "--", "/dev/null", path],
        &[1],
    )
    .await
}

pub async fn git_diff_base_content(
    working_dir: &str,
    path: &str,
    staged: bool,
) -> Result<String, String> {
    let object_spec = if staged {
        format!("HEAD:{path}")
    } else {
        format!(":{path}")
    };
    git_show_blob_or_empty(working_dir, &object_spec).await
}

pub async fn git_current_branch(working_dir: &str) -> Result<String, String> {
    match run_git_readonly(working_dir, &["rev-parse", "--abbrev-ref", "HEAD"]).await {
        Ok(branch) => Ok(branch.trim().to_string()),
        Err(e) => {
            if e.contains("not a git repository") {
                return Err(e);
            }
            // Empty repos have no HEAD yet; symbolic-ref still works
            let symbolic =
                run_git_readonly(working_dir, &["symbolic-ref", "--short", "HEAD"]).await?;
            Ok(symbolic.trim().to_string())
        }
    }
}

pub async fn git_worktree_add(working_dir: &str, path: &str, branch: &str) -> Result<(), String> {
    let local_path = parse_remote_path(path).map_or(path.to_string(), |r| r.path);
    run_git(working_dir, &["worktree", "add", "-b", branch, &local_path]).await?;
    Ok(())
}

pub async fn git_worktree_remove(working_dir: &str, path: &str) -> Result<(), String> {
    let local_path = parse_remote_path(path).map_or(path.to_string(), |r| r.path);
    run_git(working_dir, &["worktree", "remove", "--force", &local_path]).await?;
    Ok(())
}

pub async fn git_discard(working_dir: &str, paths: &[String]) -> Result<(), String> {
    let path_refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();

    let mut checkout_args = vec!["checkout", "--"];
    checkout_args.extend(path_refs.iter());
    let checkout_err = run_git(working_dir, &checkout_args).await.err();

    let mut clean_args = vec!["clean", "-f", "--"];
    clean_args.extend(path_refs.iter());
    let clean_err = run_git(working_dir, &clean_args).await.err();

    if let Some(err) = checkout_err.or(clean_err) {
        return Err(err);
    }

    Ok(())
}
