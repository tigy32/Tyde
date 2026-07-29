//! Specification-only tool mapping, shared by every ACP adapter.
//!
//! ACP standardizes a tool call's `kind` (`read`, `edit`, `execute`, …) but
//! leaves `rawInput` and `rawOutput` entirely up to the agent. These defaults
//! therefore classify on `kind` and emit a structured Tyde payload only when
//! the fields they need are actually present — an unknown agent's `edit` with
//! no recognizable path becomes `Other` carrying its raw arguments, not a
//! `ModifyFile` with an empty path.

use std::path::Path;

use serde_json::{Value, json};

use super::AcpToolCallCompletion;

/// Field names an agent might use for a shell command's working directory.
const WORKING_DIR_KEYS: &[&str] = &["working_dir", "workingDir", "cwd", "working_directory"];
/// Field names an agent might use for the replacement text of an edit.
const EDIT_AFTER_KEYS: &[&str] = &["newStr", "new_str", "file_text", "content", "newText"];
/// Field names an agent might use for the original text of an edit.
const EDIT_BEFORE_KEYS: &[&str] = &["oldStr", "old_str", "oldText"];

fn first_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
}

/// Resolve a possibly-relative tool path against the workspace root.
pub fn resolve_tool_file_path(file_path: &str, workspace_root: &str) -> String {
    if file_path.is_empty() {
        return String::new();
    }
    if Path::new(file_path).is_absolute() || workspace_root.is_empty() {
        return file_path.to_string();
    }
    Path::new(workspace_root)
        .join(file_path)
        .to_string_lossy()
        .to_string()
}

/// Line-count delta between two versions of a file.
pub fn estimate_line_diff_counts(before: &str, after: &str) -> (u64, u64) {
    if before == after {
        return (0, 0);
    }
    let before_lines = before.lines().count() as i64;
    let after_lines = after.lines().count() as i64;
    if after_lines >= before_lines {
        ((after_lines - before_lines) as u64, 0)
    } else {
        (0, (before_lines - after_lines) as u64)
    }
}

/// Collect the file paths an ACP `read` call refers to.
///
/// Handles the single-path form and the batched `ops` form; returns empty when
/// neither is recognizable.
fn read_paths(args: &Value) -> Vec<String> {
    if let Some(ops) = args.get("ops").and_then(Value::as_array) {
        let paths: Vec<String> = ops
            .iter()
            .filter_map(|op| op.get("path").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        if !paths.is_empty() {
            return paths;
        }
    }
    first_str(args, &["path", "file_path", "filePath"])
        .map(|path| vec![path.to_string()])
        .unwrap_or_default()
}

/// See [`super::adapter::AcpAgentAdapter::map_tool_request`].
pub async fn default_map_tool_request(kind: &str, args: &Value, workspace_root: &str) -> Value {
    let other = || {
        json!({
            "kind": "Other",
            "args": args,
        })
    };

    match kind {
        "execute" => {
            let Some(command) = args
                .get("command")
                .and_then(Value::as_str)
                .filter(|command| !command.is_empty())
            else {
                return other();
            };
            let working_directory = first_str(args, WORKING_DIR_KEYS)
                .unwrap_or(workspace_root)
                .to_string();
            json!({
                "kind": "RunCommand",
                "command": command,
                "working_directory": working_directory,
            })
        }
        "edit" => {
            let Some(file_path) =
                first_str(args, &["path", "file_path", "filePath"]).filter(|path| !path.is_empty())
            else {
                return other();
            };
            let after = first_str(args, EDIT_AFTER_KEYS).unwrap_or("").to_string();
            let mut before = first_str(args, EDIT_BEFORE_KEYS).unwrap_or("").to_string();

            // An agent that sends only the replacement text still produces a
            // useful diff if we can read what is on disk right now.
            let resolved = resolve_tool_file_path(file_path, workspace_root);
            if before.is_empty()
                && !resolved.is_empty()
                && Path::new(&resolved).exists()
                && let Ok(contents) = tokio::fs::read_to_string(&resolved).await
            {
                before = contents;
            }

            json!({
                "kind": "ModifyFile",
                "file_path": file_path,
                "before": before,
                "after": after,
            })
        }
        "read" => {
            let file_paths = read_paths(args);
            if file_paths.is_empty() {
                return other();
            }
            json!({
                "kind": "ReadFiles",
                "file_paths": file_paths,
            })
        }
        _ => other(),
    }
}

/// See [`super::adapter::AcpAgentAdapter::map_tool_result`].
pub fn default_map_tool_result(
    completion: &AcpToolCallCompletion,
    _request_payload: Option<&Value>,
) -> Value {
    if !completion.success {
        return error_result(completion);
    }
    // `rawOutput` is agent-defined; reporting it verbatim is the most an
    // adapter can honestly say without knowing the agent.
    json!({
        "kind": "Other",
        "result": completion.tool_result,
    })
}

/// Shared failure payload. Every adapter reports errors the same way.
pub fn error_result(completion: &AcpToolCallCompletion) -> Value {
    let short_message = completion
        .error
        .clone()
        .unwrap_or_else(|| format!("{} failed", completion.tool_name));
    let detailed_message = serde_json::to_string_pretty(&completion.tool_result)
        .unwrap_or_else(|_| completion.tool_result.to_string());
    json!({
        "kind": "Error",
        "short_message": short_message,
        "detailed_message": detailed_message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn execute_without_command_falls_back_to_other() {
        let args = json!({ "somethingElse": "x" });
        let mapped = default_map_tool_request("execute", &args, "/ws").await;
        assert_eq!(mapped["kind"], "Other");
        assert_eq!(mapped["args"], args);
    }

    #[tokio::test]
    async fn edit_without_recognizable_path_falls_back_to_other() {
        let args = json!({ "newStr": "hello" });
        let mapped = default_map_tool_request("edit", &args, "/ws").await;
        assert_eq!(
            mapped["kind"], "Other",
            "an unrecognized edit must not become a ModifyFile with an empty path"
        );
    }

    #[tokio::test]
    async fn execute_uses_workspace_root_when_no_working_dir() {
        let args = json!({ "command": "ls" });
        let mapped = default_map_tool_request("execute", &args, "/ws").await;
        assert_eq!(mapped["kind"], "RunCommand");
        assert_eq!(mapped["command"], "ls");
        assert_eq!(mapped["working_directory"], "/ws");
    }

    #[tokio::test]
    async fn read_accepts_both_single_path_and_ops_forms() {
        let single = default_map_tool_request("read", &json!({ "path": "a.rs" }), "/ws").await;
        assert_eq!(single["file_paths"], json!(["a.rs"]));

        let ops = default_map_tool_request(
            "read",
            &json!({ "ops": [{ "path": "a.rs" }, { "path": "b.rs" }] }),
            "/ws",
        )
        .await;
        assert_eq!(ops["file_paths"], json!(["a.rs", "b.rs"]));
    }

    #[test]
    fn failed_completion_maps_to_error_regardless_of_kind() {
        let completion = AcpToolCallCompletion {
            tool_call_id: "t1".to_string(),
            tool_name: "grep".to_string(),
            kind: "execute".to_string(),
            success: false,
            tool_result: json!({ "items": [] }),
            error: Some("boom".to_string()),
        };
        let mapped = default_map_tool_result(&completion, None);
        assert_eq!(mapped["kind"], "Error");
        assert_eq!(mapped["short_message"], "boom");
    }

    #[test]
    fn line_diff_counts_report_net_change() {
        assert_eq!(estimate_line_diff_counts("a\nb", "a\nb"), (0, 0));
        assert_eq!(estimate_line_diff_counts("a", "a\nb\nc"), (2, 0));
        assert_eq!(estimate_line_diff_counts("a\nb\nc", "a"), (0, 2));
    }
}
