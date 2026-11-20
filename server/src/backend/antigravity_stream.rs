//! Helpers shared between Antigravity's ACP adapter, trajectory decoder, and
//! out-of-band `/usage` capacity probe.

use serde::Deserialize;
use serde_json::Value;

use protocol::{AgentExecutionMode, AskUserQuestion, AskUserQuestionOption, ToolRequestType};

/// One line of `agy --output-format stream-json -p /usage` stdout.
#[derive(Debug, Clone)]
pub enum AgyFrame {
    CommandResult(Value),
    Other,
}

pub fn parse_frame(line: &str) -> Result<AgyFrame, String> {
    let value: Value = serde_json::from_str(line)
        .map_err(|err| format!("Antigravity emitted a line that is not JSON: {err}"))?;
    let event = value
        .get("event")
        .and_then(Value::as_str)
        .ok_or_else(|| "Antigravity event is missing its \"event\" field".to_string())?;
    match event {
        "command_result" => Ok(AgyFrame::CommandResult(
            value.get("command").cloned().unwrap_or(Value::Null),
        )),
        _ => Ok(AgyFrame::Other),
    }
}

/// The exit code `agy` records in command output text when no structured
/// `exit_code` field is present.
pub fn exit_code_from_result(result: &str) -> Option<i32> {
    for marker in ["The command exited with code ", "Command exited with code "] {
        if let Some(pos) = result.find(marker) {
            let rest = &result[pos + marker.len()..];
            let end = rest
                .find(|c: char| !c.is_ascii_digit() && c != '-')
                .unwrap_or(rest.len());
            if let Ok(code) = rest[..end].parse::<i32>() {
                return Some(code);
            }
        }
    }
    None
}

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.is_empty())
}

/// Maps one `agy` tool call onto Tyde's normalized tool vocabulary.
pub fn tool_request_type(tool_name: &str, args: &Value, enriched: bool) -> ToolRequestType {
    match tool_name {
        "invoke_subagent" => ToolRequestType::AgentSpawn {
            prompt: arg_str(args, "Prompt"),
            name: arg_str(args, "Role"),
            execution_mode: AgentExecutionMode::Background,
        },
        MCP_DISPATCH_TOOL => {
            let (inner_name, inner_args) = mcp_inner_call(args);
            tool_request_type(&inner_name, &inner_args, true)
        }
        "run_command" => ToolRequestType::RunCommand {
            command: arg_str(args, "CommandLine").unwrap_or_default(),
            working_directory: arg_str(args, "Cwd").unwrap_or_default(),
        },
        "view_file" => ToolRequestType::ReadFiles {
            file_paths: arg_str(args, "AbsolutePath").into_iter().collect(),
        },
        "read_file" if arg_str(args, "Path").is_some() => ToolRequestType::ReadFiles {
            file_paths: arg_str(args, "Path").into_iter().collect(),
        },
        "edit_file"
            if enriched
                && arg_str(args, "Path").is_some()
                && arg_str(args, "Target").is_some()
                && arg_str(args, "Replacement").is_some() =>
        {
            ToolRequestType::ModifyFile {
                file_path: arg_str(args, "Path").unwrap_or_default(),
                before: arg_str(args, "Target").unwrap_or_default(),
                after: arg_str(args, "Replacement").unwrap_or_default(),
            }
        }
        "write_to_file" if enriched => ToolRequestType::ModifyFile {
            file_path: arg_str(args, "TargetFile").unwrap_or_default(),
            before: String::new(),
            after: arg_str(args, "CodeContent").unwrap_or_default(),
        },
        "replace_file_content" | "multi_replace_file_content" | "sed_file" if enriched => {
            ToolRequestType::ModifyFile {
                file_path: arg_str(args, "TargetFile").unwrap_or_default(),
                before: arg_str(args, "TargetContent").unwrap_or_default(),
                after: arg_str(args, "ReplacementContent").unwrap_or_default(),
            }
        }
        "ask_question" => ToolRequestType::AskUserQuestion {
            mode: protocol::UserQuestionMode::NonBlocking,
            questions: args
                .get("questions")
                .and_then(Value::as_array)
                .map(|questions| questions.iter().map(ask_user_question).collect())
                .unwrap_or_default(),
        },
        "search_web" => ToolRequestType::WebSearch {
            query: arg_str(args, "query")
                .or_else(|| arg_str(args, "Query"))
                .unwrap_or_default(),
        },
        "generate_image" => ToolRequestType::GenerateImage {
            prompt: arg_str(args, "Prompt").or_else(|| arg_str(args, "prompt")),
        },
        "wait" | "wait_5_seconds" => ToolRequestType::Sleep {
            duration_ms: args
                .get("DurationMs")
                .and_then(Value::as_u64)
                .or_else(|| {
                    args.get("Seconds")
                        .and_then(Value::as_u64)
                        .map(|s| s.saturating_mul(1000))
                })
                .unwrap_or(5_000),
        },
        _ => ToolRequestType::Other { args: args.clone() },
    }
}

fn ask_user_question(raw: &Value) -> AskUserQuestion {
    AskUserQuestion {
        id: None,
        question: arg_str(raw, "question").unwrap_or_default(),
        header: None,
        options: raw
            .get("options")
            .and_then(Value::as_array)
            .map(|options| {
                options
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|label| AskUserQuestionOption {
                        label: label.to_owned(),
                        description: None,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        answer_schema: None,
        multi_select: raw
            .get("is_multi_select")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// Dispatcher tool `agy` uses for lazy-loaded MCP tools.
pub const MCP_DISPATCH_TOOL: &str = "call_mcp_tool";

/// The tool name and arguments inside a `call_mcp_tool` dispatch.
pub fn mcp_inner_call(args: &Value) -> (String, Value) {
    let tool = arg_str(args, "ToolName").unwrap_or_default();
    let server = arg_str(args, "ServerName").unwrap_or_default();
    let qualified = if server.is_empty() || server == crate::mcp_bridge::MANAGED_SERVER_NAME {
        tool
    } else {
        format!("{server}__{tool}")
    };
    let inner = args.get("Arguments").cloned().unwrap_or(Value::Null);
    (qualified, inner)
}

// ── Capacity ────────────────────────────────────────────────────────────────

/// The payload `agy -p "/usage"` answers with.
#[derive(Debug, Clone, Deserialize)]
pub struct AgyUsageReport {
    #[serde(default)]
    pub groups: Vec<AgyUsageGroup>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgyUsageGroup {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub buckets: Vec<AgyUsageBucket>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgyUsageBucket {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub window: String,
    #[serde(default)]
    pub remaining_fraction: Option<f64>,
    #[serde(default)]
    pub reset_time: Option<String>,
}

/// Pulls the `/usage` payload out of a `command_result` frame.
pub fn usage_report_from_frame(frame: &AgyFrame) -> Option<AgyUsageReport> {
    let command = match frame {
        AgyFrame::CommandResult(command) => command,
        AgyFrame::Other => return None,
    };
    if command.get("name").and_then(Value::as_str) != Some("usage") {
        return None;
    }
    serde_json::from_value::<AgyUsageReport>(command.get("data")?.clone()).ok()
}
