use std::path::Path;

use base64::Engine as _;
use protocol::{
    FileInfo, NativeTool, NativeToolKind, NativeToolSelection, ToolCategory, ToolExecutionOutcome,
    ToolExecutionResult, ToolRequestType,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    transport::Connection,
    wire::{Item, ItemStatus},
};

pub(super) const NATIVE_TOOLS: &[&str] = &[
    "workflow",
    "read_file",
    "search",
    "write_file",
    "edit_file",
    "read_memory",
    "add_memory",
    "edit_memory",
    "work_stop",
    "work_list",
    "web_fetch",
    "web_search",
    "bash",
    "bash_input",
    "monitor",
    "cron_create",
    "cron_delete",
    "cron_list",
    "get_goal",
    "create_goal",
    "update_goal",
    "report_progress",
    "snooze_reminder",
    "write_todos",
    "request_user_input",
    "subagent_spawn",
    "subagent_status",
    "subagent_send_message",
    "subagent_wait",
    "subagent_read_result",
    "subagent_cancel",
    "read_skill",
];

pub(super) fn kind(name: &str) -> NativeToolKind {
    match name {
        "request_user_input" => NativeToolKind::Controlled {
            category: ToolCategory::AskUser,
        },
        "workflow" | "subagent_spawn" => NativeToolKind::Controlled {
            category: ToolCategory::AgentDelegation,
        },
        "write_file" | "edit_file" | "add_memory" | "edit_memory" => NativeToolKind::FileMutation,
        "bash" | "bash_input" | "monitor" => NativeToolKind::CommandExecution,
        "read_file"
        | "search"
        | "read_memory"
        | "web_fetch"
        | "web_search"
        | "read_skill"
        | "subagent_status"
        | "subagent_wait"
        | "subagent_read_result"
        | "work_list"
        | "get_goal"
        | "cron_list" => NativeToolKind::ReadOnly,
        "work_stop"
        | "create_goal"
        | "update_goal"
        | "report_progress"
        | "snooze_reminder"
        | "write_todos"
        | "subagent_send_message"
        | "subagent_cancel"
        | "cron_create"
        | "cron_delete" => NativeToolKind::SessionControl,
        _ => NativeToolKind::Unknown,
    }
}

/// An audited Muse host, identified by the MSP schema fingerprint it serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Host {
    V1_4_6,
    V1_4_7,
    V1_4_10,
}

pub(super) const HOSTS: &[(&str, Host)] = &[
    (
        "sha256:eae467ab60cb9a5d5af4de616830f065e59375c987fd59269c8cc33ed2a1137f",
        Host::V1_4_6,
    ),
    (
        "sha256:6de32db08bbbe0deb478d2bfcaba001e9fb490986bc80db3aceb4d996abf7df9",
        Host::V1_4_7,
    ),
    (
        "sha256:5d3b43e5ccdb7dc4f5a32c380f5f2cde0f10e3059b93610afb2e73f73b1d914c",
        Host::V1_4_10,
    ),
];

pub(super) fn host(fingerprint: &str) -> Option<Host> {
    HOSTS
        .iter()
        .find(|(known, _)| *known == fingerprint)
        .map(|(_, host)| *host)
}

pub(super) fn selection(name: &str, named_selectors: &[String], host: Host) -> NativeToolSelection {
    // Every audited host exposes these in its default catalog but rejects
    // their names in run.toolset. 1.4.6 and 1.4.7 accept read_skill there;
    // 1.4.10 rejects it as an unknown tool name.
    match name {
        "snooze_reminder" | "write_todos" => NativeToolSelection::DefaultOnly,
        "read_skill" if host == Host::V1_4_10 => NativeToolSelection::DefaultOnly,
        // The audited mixed named configuration silently omitted these tools.
        // Quarantine that combination without narrowing the default toolset.
        "subagent_spawn"
        | "subagent_status"
        | "subagent_send_message"
        | "subagent_wait"
        | "subagent_read_result"
        | "subagent_cancel" => NativeToolSelection::NamedWithoutMcp,
        name if NATIVE_TOOLS.contains(&name)
            || named_selectors.iter().any(|known| known.as_str() == name) =>
        {
            NativeToolSelection::NamedSelectable
        }
        _ => NativeToolSelection::Unverified,
    }
}

/// Policy admission happens before the host is known, so it refuses only
/// selectors that no audited host accepts. A selector some hosts reject is
/// refused when the launched host's named toolset is derived.
pub(super) fn admission_selection(name: &str) -> NativeToolSelection {
    HOSTS
        .iter()
        .map(|(_, host)| selection(name, &[], *host))
        .find(|selection| *selection != NativeToolSelection::DefaultOnly)
        .unwrap_or(NativeToolSelection::DefaultOnly)
}

pub(super) fn descriptor(name: String, named_selectors: &[String], host: Host) -> NativeTool {
    NativeTool {
        kind: kind(&name),
        selection: selection(&name, named_selectors, host),
        name,
    }
}

pub(super) fn mcp_selector_reference(name: &str) -> bool {
    name.strip_prefix("mcp__")
        .and_then(|qualified| qualified.split_once("__"))
        .is_some_and(|(server, tool)| !server.is_empty() && !tool.is_empty())
}

pub(super) fn question_only_policy(policy: &protocol::ToolPolicy) -> bool {
    match policy {
        protocol::ToolPolicy::Unrestricted => true,
        protocol::ToolPolicy::DenyList { tools } => tools
            .iter()
            .flat_map(|name| policy_names(name))
            .all(|name| name == "request_user_input"),
        protocol::ToolPolicy::AllowList { .. } => false,
    }
}

pub(super) fn policy_names(name: &str) -> Vec<String> {
    match name {
        "Read" => vec!["read_file".to_owned()],
        "LS" | "Glob" | "Grep" => vec!["search".to_owned()],
        "Bash" | "exec_command" => vec!["bash".to_owned(), "bash_input".to_owned()],
        _ => vec![name.to_owned()],
    }
}

pub(super) fn request(name: &str, args: &Value, cwd: &str) -> Result<ToolRequestType, String> {
    if missing_request_field(name, args).is_some() {
        return Ok(ToolRequestType::Other { args: args.clone() });
    }
    if let Some(request) =
        crate::backend::agent_control_progress::tyde_tool_request_type(name, args)
    {
        return Ok(request);
    }
    Ok(match name {
        "subagent_spawn" => ToolRequestType::AgentSpawn {
            prompt: Some(required(args, "objective")?.to_owned()),
            name: args.get("role").and_then(Value::as_str).map(str::to_owned),
            execution_mode: protocol::AgentExecutionMode::Background,
        },
        "read_file" => ToolRequestType::ReadFiles {
            file_paths: vec![path(args, cwd)?],
        },
        "bash" => ToolRequestType::RunCommand {
            command: required(args, "command")?.to_owned(),
            working_directory: String::new(),
        },
        "web_search" => ToolRequestType::WebSearch {
            query: required(args, "query")?.to_owned(),
        },
        // Native structured patches supply file-edit cards, after they are recorded.
        _ => ToolRequestType::Other { args: args.clone() },
    })
}

fn missing_request_field(name: &str, args: &Value) -> Option<&'static str> {
    let fields: &[&str] = match name {
        "subagent_spawn" => &["objective"],
        "read_file" => &["path"],
        "bash" => &["command"],
        "web_search" => &["query"],
        "write_file" => &["path", "content"],
        "edit_file" => &["path", "find", "replace"],
        _ => &[],
    };
    fields
        .iter()
        .copied()
        .find(|field| args.get(field).and_then(Value::as_str).is_none())
}

pub(super) fn path(args: &Value, cwd: &str) -> Result<String, String> {
    let path = Path::new(required(args, "path")?);
    Ok(if path.is_absolute() {
        path.to_owned()
    } else {
        Path::new(cwd).join(path)
    }
    .to_string_lossy()
    .into_owned())
}

pub(super) fn required<'a>(args: &'a Value, field: &str) -> Result<&'a str, String> {
    args.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Muse native tool omitted its {field} argument"))
}

#[derive(Deserialize)]
struct Patch {
    files: Vec<FilePatch>,
}
#[derive(Deserialize)]
struct FilePatch {
    path: String,
    hunks: Vec<Hunk>,
}
#[derive(Deserialize)]
struct Hunk {
    lines: Vec<String>,
}

pub(super) async fn edit_request(
    connection: &Connection,
    session_id: &str,
    item: &Item,
    args: &Value,
    cwd: &str,
) -> Result<ToolRequestType, String> {
    let name = item
        .tool
        .as_deref()
        .ok_or("Muse edit item omitted its tool name")?;
    if item.status != ItemStatus::Completed && missing_request_field(name, args).is_some() {
        return Ok(ToolRequestType::Other { args: args.clone() });
    }
    let Some(reference) = &item.patch_ref else {
        let (before, after) = match name {
            "write_file" => (String::new(), required(args, "content")?.to_owned()),
            "edit_file" => (
                required(args, "find")?.to_owned(),
                required(args, "replace")?.to_owned(),
            ),
            _ => return Err("Muse emitted a patch for an unsupported tool family".to_owned()),
        };
        return Ok(ToolRequestType::ModifyFile {
            file_path: path(args, cwd)?,
            before,
            after,
        });
    };
    let bytes = read_output(
        connection,
        session_id,
        &item.item_id,
        &reference.id,
        reference.byte_len,
    )
    .await?;
    let patch: Patch = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Invalid Muse native patch: {error}"))?;
    let [file] = patch.files.as_slice() else {
        return Err("Muse file tool emitted a patch with other than one file".to_owned());
    };
    let mut before = String::new();
    let mut after = String::new();
    for hunk in &file.hunks {
        for line in &hunk.lines {
            let Some((prefix, text)) = line.split_at_checked(1) else {
                return Err("Muse patch contained an empty hunk line".to_owned());
            };
            match prefix {
                " " => {
                    before.push_str(text);
                    before.push('\n');
                    after.push_str(text);
                    after.push('\n');
                }
                "-" => {
                    before.push_str(text);
                    before.push('\n');
                }
                "+" => {
                    after.push_str(text);
                    after.push('\n');
                }
                "\\" => {}
                _ => return Err("Muse patch contained an unrecognized hunk line".to_owned()),
            }
        }
    }
    Ok(ToolRequestType::ModifyFile {
        file_path: if Path::new(&file.path).is_absolute() {
            file.path.clone()
        } else {
            Path::new(cwd)
                .join(&file.path)
                .to_string_lossy()
                .into_owned()
        },
        before,
        after,
    })
}

pub(super) async fn read_output(
    connection: &Connection,
    session_id: &str,
    item_id: &str,
    id: &str,
    byte_len: u64,
) -> Result<Vec<u8>, String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Output {
        content: String,
        encoding: String,
        byte_len: u64,
        offset_bytes: u64,
        eof: bool,
    }
    let mut bytes = Vec::new();
    let mut offset = 0_u64;
    loop {
        let result: Output = connection.typed("item/readOutput", json!({"sessionId":session_id,"itemId":item_id,"outputRef":id,"offsetBytes":offset,"lengthBytes":65536})).await?;
        if result.offset_bytes != offset || (!result.eof && result.byte_len == 0) {
            return Err("Muse stored-output cursor did not advance contiguously".to_owned());
        }
        use base64::Engine;
        let part = match result.encoding.as_str() {
            "base64" => base64::engine::general_purpose::STANDARD
                .decode(&result.content)
                .map_err(|error| format!("Muse stored output was invalid base64: {error}"))?,
            "utf8" => result.content.into_bytes(),
            _ => return Err("Muse used an unsupported stored-output encoding".to_owned()),
        };
        if part.len() as u64 != result.byte_len {
            return Err("Muse stored-output byte count was inconsistent".to_owned());
        }
        bytes.extend(part);
        offset += result.byte_len;
        if result.eof {
            break;
        }
    }
    if offset != byte_len {
        return Err("Muse stored output did not match its native reference length".to_owned());
    }
    Ok(bytes)
}

pub(super) async fn outcome(
    connection: &Connection,
    session_id: &str,
    item: &Item,
    request: &ToolRequestType,
    command: Option<&super::wire::CommandResult>,
    replaying: bool,
) -> Result<ToolExecutionOutcome, String> {
    match item.status {
        ItemStatus::Cancelled => {
            return Ok(ToolExecutionOutcome::Cancelled {
                message: match &item.failure_reason {
                    Some(reason) => reason.clone(),
                    None => "Muse cancelled the native tool".to_owned(),
                },
            });
        }
        ItemStatus::Failed | ItemStatus::Rejected | ItemStatus::TimedOut => {
            return Ok(ToolExecutionOutcome::Failed {
                message: match &item.failure_reason {
                    Some(reason) => reason.clone(),
                    None => "Muse rejected or failed the native tool".to_owned(),
                },
                details: None,
                normalization_failure: None,
                result: None,
            });
        }
        ItemStatus::InProgress => {
            return Err("Muse attempted to complete an in-progress tool".to_owned());
        }
        ItemStatus::Completed => {}
    }
    let result = match request {
        ToolRequestType::ModifyFile { .. } => {
            let summary = item
                .patch_summary
                .as_ref()
                .ok_or("Successful Muse file edit omitted its native patch summary")?;
            ToolExecutionResult::ModifyFile {
                lines_added: summary.added,
                lines_removed: summary.removed,
            }
        }
        ToolRequestType::ReadFiles { file_paths } => {
            let files = file_paths
                .iter()
                .map(|path| FileInfo {
                    path: path.clone(),
                    bytes: None,
                })
                .collect();
            tracing::debug!(
                replaying,
                files = file_paths.len(),
                "Muse file-read result has no retained read-time byte counts"
            );
            ToolExecutionResult::ReadFiles { files }
        }
        ToolRequestType::RunCommand { .. } => {
            let command =
                command.ok_or("Muse command completed without its native terminal result")?;
            ToolExecutionResult::RunCommand {
                exit_code: command.exit_code,
                stdout: command.output.clone(),
                stderr: String::new(),
            }
        }
        ToolRequestType::WebSearch { .. } => ToolExecutionResult::WebSearch,
        ToolRequestType::ViewImage { .. } => ToolExecutionResult::ViewImage {
            image: None,
            preview_error: None,
        },
        _ => {
            if let ToolRequestType::Other { args } = request {
                let name = item
                    .tool
                    .as_deref()
                    .ok_or("Muse tool completion omitted its native name")?;
                if let Some(field) = missing_request_field(name, args) {
                    return Err(format!(
                        "Muse completed a native tool without its {field} argument"
                    ));
                }
            }
            let output = if item.truncated == Some(true) {
                let reference = item
                    .output_ref
                    .as_ref()
                    .ok_or("Muse truncated a tool result without a stored output reference")?;
                String::from_utf8(
                    read_output(
                        connection,
                        session_id,
                        &item.item_id,
                        &reference.id,
                        reference.byte_len,
                    )
                    .await?,
                )
                .map_err(|_| "Muse text tool output was not UTF-8".to_owned())?
            } else {
                item.visible_output
                    .clone()
                    .ok_or("Muse completed a tool without its native result text")?
            };
            ToolExecutionResult::Other {
                result: Value::String(output),
            }
        }
    };
    Ok(ToolExecutionOutcome::Succeeded { result })
}

pub(super) fn mcp_outcome(
    item: &Item,
    text: String,
    images: Vec<super::wire::ModelVisibleContent>,
) -> Result<ToolExecutionOutcome, String> {
    let name = item
        .tool
        .as_deref()
        .ok_or("Muse MCP completion omitted its native name")?;
    let mut native_images = Vec::new();
    for content in images {
        let super::wire::ModelVisibleContent::Image {
            base64_data,
            media_type,
        } = content;
        let base64_data = base64_data.ok_or("Muse MCP image omitted its retained inline pixels")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&base64_data)
            .map_err(|_| "Muse MCP image used invalid base64".to_owned())?;
        if bytes.is_empty() || !media_type.starts_with("image/") {
            return Err("Muse MCP image omitted its pixels or image media type".to_owned());
        }
        native_images.push(protocol::ImageData {
            media_type,
            data: base64_data,
        });
    }
    let result = ToolExecutionResult::NativeContent {
        text: text.clone(),
        images: native_images,
    };
    if item.status == ItemStatus::Failed {
        return Ok(ToolExecutionOutcome::Failed {
            message: item
                .failure_reason
                .clone()
                .ok_or("Muse MCP failure omitted its native reason")?,
            details: None,
            normalization_failure: None,
            result: Some(result),
        });
    }
    Ok(
        match crate::backend::agent_control_progress::tyde_tool_result(name, &Value::String(text)) {
            Ok(typed) => ToolExecutionOutcome::Succeeded {
                result: match typed {
                    Some(typed) => typed,
                    None => result,
                },
            },
            Err(error) => ToolExecutionOutcome::Failed {
                message: error.to_string(),
                details: Some(error.detail),
                normalization_failure: Some(error.normalization_failure),
                result: Some(result),
            },
        },
    )
}
