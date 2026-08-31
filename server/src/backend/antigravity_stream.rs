//! The `agy` NDJSON stream-json protocol, and the mapping from its steps to
//! Tyde chat events.
//!
//! Antigravity used to run one `agy -p "<prompt>"` process per turn and treat
//! the whole of stdout as the assistant's answer. Text print mode emits nothing
//! but the final message — measured 2026-08-25 against agy 1.1.20, a turn that
//! read a file and ran a command printed 128 bytes and not one word about
//! either — so the backend could not emit a single tool card, report a token,
//! or learn its own conversation id without scraping the CLI's log file.
//!
//! `--input-format stream-json --output-format stream-json` (agy >= 1.1.15)
//! replaces all of that: one long-lived process, one turn per NDJSON line on
//! stdin, and a typed event stream out.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use protocol::{AgentExecutionMode, AskUserQuestion, AskUserQuestionOption, ToolRequestType};

/// One line of `agy`'s stdout.
///
/// The vocabulary is closed and documented as such, but a `result` carrying an
/// unrecognised `event` is still the provider telling us something we do not
/// understand, so it is surfaced rather than dropped — see `Unrecognized`.
#[derive(Debug, Clone)]
pub enum AgyFrame {
    Init(AgyInit),
    Step(AgyStep),
    Result(AgyResult),
    /// `-p "/usage"`-style structured payloads. They ride the same stream but
    /// never belong to a turn.
    CommandResult(Value),
    Unrecognized {
        event: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgyInit {
    pub conversation_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgyResult {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub response: String,
    #[serde(default)]
    pub error: Option<String>,
    /// Session-cumulative, NOT per turn. Measured: turn 2 reported 1209 output
    /// tokens, which is turn 1's 849 plus turn 2's own 344 + 16. Using this as
    /// the turn total is the defect `assert_turn_is_not_the_running_total`
    /// exists to catch, so turn totals are summed from the step usages instead
    /// and this field is only ever used to cross-check the cumulative scope.
    #[serde(default)]
    pub usage: Option<AgyUsage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgyStep {
    pub step_index: u64,
    #[serde(default)]
    pub step_type: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub text_delta: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_info: Option<AgyToolInfo>,
    #[serde(default)]
    pub subagent_info: Option<AgySubagentInfo>,
    #[serde(default)]
    pub usage: Option<AgyUsage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgyToolInfo {
    #[serde(default)]
    pub name: Option<String>,
    /// A *summary* of the call, not the call. `replace_file_content` reports
    /// only `{"TargetFile": ...}` here; the text it replaced and the text it
    /// wrote live in the on-disk transcript. See [`TranscriptReader`].
    #[serde(default)]
    pub parameters: Option<Value>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub error: Option<AgyToolError>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgyToolError {
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgySubagentInfo {
    #[serde(default)]
    pub subagents: Vec<AgySubagent>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgySubagent {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub type_name: Option<String>,
    #[serde(default)]
    pub initial_prompt: Option<String>,
    #[serde(default)]
    pub conversation_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
pub struct AgyUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub thinking_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

impl AgyUsage {
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            thinking_tokens: self.thinking_tokens.saturating_add(other.thinking_tokens),
            cache_read_tokens: self
                .cache_read_tokens
                .saturating_add(other.cache_read_tokens),
            total_tokens: self.total_tokens.saturating_add(other.total_tokens),
        }
    }

    pub fn to_protocol(self) -> protocol::TokenUsage {
        protocol::TokenUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            total_tokens: self.total_tokens,
            cached_prompt_tokens: Some(self.cache_read_tokens),
            cache_creation_input_tokens: None,
            reasoning_tokens: Some(self.thinking_tokens),
        }
    }
}

/// The step states `agy` reports. `ACTIVE` genuinely precedes execution —
/// measured as a 6.05s `ACTIVE`→`DONE` gap on a `sleep 6` command — so a tool
/// card opened here has a real running window rather than being backfilled.
pub const STATE_ACTIVE: &str = "ACTIVE";
pub const STATE_DONE: &str = "DONE";
pub const STATE_ERROR: &str = "ERROR";
pub const STATE_CANCELLED: &str = "CANCELLED";

pub const STEP_AGENT_RESPONSE: &str = "agent_response";
pub const STEP_TOOL: &str = "tool";
pub const STEP_SUBAGENT: &str = "subagent";
pub const STEP_USER_INPUT: &str = "user_input";
pub const STEP_CHECKPOINT: &str = "checkpoint";
pub const STEP_SYSTEM_MESSAGE: &str = "system_message";
/// What `agy` reports for a step whose type has no public name. Measured: an
/// `ask_question` call, which headless mode answers with "A1: User Skipped"
/// before any consumer could route it to a human.
pub const STEP_UNKNOWN: &str = "unknown";

pub fn parse_frame(line: &str) -> Result<AgyFrame, String> {
    let value: Value = serde_json::from_str(line)
        .map_err(|err| format!("Antigravity emitted a line that is not JSON: {err}"))?;
    let event = value
        .get("event")
        .and_then(Value::as_str)
        .ok_or_else(|| "Antigravity event is missing its \"event\" field".to_string())?;
    match event {
        "init" => {
            let conversation_id = value
                .get("conversation_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| "Antigravity init event carried no conversation_id".to_string())?;
            Ok(AgyFrame::Init(AgyInit {
                conversation_id: conversation_id.to_string(),
            }))
        }
        "step_update" => {
            let step = value
                .get("step_update")
                .ok_or_else(|| "Antigravity step_update event has no payload".to_string())?;
            serde_json::from_value::<AgyStep>(step.clone())
                .map(AgyFrame::Step)
                .map_err(|err| format!("Antigravity step_update is malformed: {err}"))
        }
        "result" => {
            let result = value
                .get("result")
                .ok_or_else(|| "Antigravity result event has no payload".to_string())?;
            serde_json::from_value::<AgyResult>(result.clone())
                .map(AgyFrame::Result)
                .map_err(|err| format!("Antigravity result is malformed: {err}"))
        }
        "command_result" => Ok(AgyFrame::CommandResult(
            value.get("command").cloned().unwrap_or(Value::Null),
        )),
        other => Ok(AgyFrame::Unrecognized {
            event: other.to_string(),
        }),
    }
}

/// The full tool arguments `agy` writes to disk but summarises out of the
/// stream.
///
/// `tool_info.parameters` carries enough to name a call and not enough to draw
/// it: a `replace_file_content` reports `{"TargetFile": "…"}` with neither the
/// text it matched nor the text it wrote, and `run_command` reports no working
/// directory and no exit code. All of it is in
/// `<brain>/<conv>/.system_generated/logs/transcript_full.jsonl`.
///
/// Correlation is by step index: a `tool` step at index N in the stream is the
/// tool's *result* step, and the `PLANNER_RESPONSE` that issued the call is at
/// N-1. Verified across two sessions — stream tool steps 3, 9, 13, 17 against
/// transcript tool calls at 2, 8, 12, 16.
///
/// This is a private file of another program, so every lookup is allowed to
/// fail. It fails *visibly*: a call whose arguments cannot be recovered still
/// gets a card, built from the stream summary alone, and the backend records
/// that the enrichment missed rather than pretending the summary was the whole
/// call.
pub struct TranscriptReader {
    path: PathBuf,
    /// step index -> every tool call issued by the `PLANNER_RESPONSE` at that
    /// index, in the order it issued them.
    calls: BTreeMap<u64, Vec<TranscriptToolCall>>,
    /// step index -> the `GENERIC` result recorded for that step.
    results: BTreeMap<u64, String>,
    misses: u64,
}

#[derive(Debug, Clone)]
pub struct TranscriptToolCall {
    pub name: String,
    pub args: Value,
}

#[derive(Debug, Clone, Deserialize)]
struct TranscriptStep {
    step_index: u64,
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<TranscriptToolCallRaw>>,
}

#[derive(Debug, Clone, Deserialize)]
struct TranscriptToolCallRaw {
    #[serde(default)]
    name: String,
    #[serde(default)]
    args: Value,
}

impl TranscriptReader {
    pub fn new(brain_dir: &Path, conversation_id: &str) -> Self {
        Self {
            path: brain_dir
                .join(conversation_id)
                .join(".system_generated")
                .join("logs")
                .join("transcript_full.jsonl"),
            calls: BTreeMap::new(),
            results: BTreeMap::new(),
            misses: 0,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// Re-reads the transcript from scratch. It is appended to as the turn runs
    /// and is small (a few KB per turn), so a reread is cheaper than tracking a
    /// file offset across the process restarts that interrupt forces.
    pub fn refresh(&mut self) {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return;
        };
        self.calls.clear();
        self.results.clear();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(step) = serde_json::from_str::<TranscriptStep>(line) else {
                continue;
            };
            if let Some(calls) = step.tool_calls
                && !calls.is_empty()
            {
                self.calls.insert(
                    step.step_index,
                    calls
                        .into_iter()
                        .map(|call| TranscriptToolCall {
                            name: call.name,
                            args: call.args,
                        })
                        .collect(),
                );
            } else if step.kind == "GENERIC"
                && let Some(content) = step.content
            {
                self.results.insert(step.step_index, content);
            }
        }
    }

    /// The call that produced the stream's `tool` step at `step_index`.
    ///
    /// A `PLANNER_RESPONSE` at step P issuing M calls is followed by exactly M
    /// result steps at P+1..=P+M, one per call and in issue order. Verified
    /// against a parallel batch: planner step 14 with three `run_command` calls
    /// produced result steps 15, 16 and 17. Matching only P = N-1 therefore
    /// recovers the first call of a batch and silently loses the rest, which is
    /// how two of those three commands lost their working directory.
    pub fn call_for_tool_step(
        &mut self,
        step_index: u64,
        tool_name: &str,
    ) -> Option<&TranscriptToolCall> {
        let planner = *self.calls.range(..step_index).next_back()?.0;
        let offset = usize::try_from(step_index - planner - 1).ok()?;
        let matches = self
            .calls
            .get(&planner)
            .and_then(|calls| calls.get(offset))
            .is_some_and(|call| call.name == tool_name);
        if !matches {
            self.misses = self.misses.saturating_add(1);
            return None;
        }
        self.calls.get(&planner).and_then(|calls| calls.get(offset))
    }

    /// The call at `step_index` without checking its name.
    ///
    /// `agy` reports some steps as `step_type: "unknown"` and gives them no
    /// `tool_name` and no `tool_info` — an `ask_question` is one — so the
    /// transcript is the only place their identity exists at all.
    pub fn unnamed_call_for_tool_step(&self, step_index: u64) -> Option<&TranscriptToolCall> {
        let planner = *self.calls.range(..step_index).next_back()?.0;
        let offset = usize::try_from(step_index - planner - 1).ok()?;
        self.calls.get(&planner).and_then(|calls| calls.get(offset))
    }

    pub fn result_for_tool_step(&self, step_index: u64) -> Option<&str> {
        self.results.get(&step_index).map(String::as_str)
    }
}

/// The exit code `agy` records in the transcript but leaves out of the stream.
pub fn exit_code_from_result(result: &str) -> Option<i32> {
    let marker = "The command exited with code ";
    let start = result.find(marker)? + marker.len();
    let rest = &result[start..];
    let end = rest.find(|c: char| !c.is_ascii_digit() && c != '-')?;
    rest[..end].parse::<i32>().ok()
}

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.is_empty())
}

/// Maps one `agy` tool call onto Tyde's normalized tool vocabulary.
///
/// `enriched` is the transcript's copy of the call when it could be recovered,
/// and the stream's summary otherwise. The distinction matters for exactly one
/// mapping: `ModifyFile` needs the before and after text, which only the
/// transcript has, so an unenriched write falls through to `Other` rather than
/// inventing an empty diff.
pub fn tool_request_type(tool_name: &str, args: &Value, enriched: bool) -> ToolRequestType {
    match tool_name {
        // Every MCP call arrives as one dispatcher tool. Unwrapping it is the
        // whole job here: the card, and the shared agent-control projection in
        // `EventStream`, both key off the tool the model actually reached for.
        // Left wrapped, Tyde's own tools reach that seam as `call_mcp_tool`
        // with the real name buried in the arguments, and nothing downstream
        // can recognise a spawn. Typing them is deliberately NOT done here —
        // the seam does it once for every backend, and only for requests still
        // carrying `Other`.
        MCP_DISPATCH_TOOL => {
            let (inner_name, inner_args) = mcp_inner_call(args);
            // An MCP call's arguments always arrive whole, so the enrichment
            // question does not apply to what is inside the dispatcher.
            tool_request_type(&inner_name, &inner_args, true)
        }
        "run_command" => ToolRequestType::RunCommand {
            command: arg_str(args, "CommandLine").unwrap_or_default(),
            working_directory: arg_str(args, "Cwd").unwrap_or_default(),
        },
        // Only the tool that returns a file's contents. `list_dir`,
        // `find_by_name` and `grep_search` name a directory and a pattern
        // rather than the files they read, so they stay `Other` — the same line
        // Claude draws, where `Read` maps and `Glob`/`Grep` do not.
        "view_file" => ToolRequestType::ReadFiles {
            file_paths: arg_str(args, "AbsolutePath").into_iter().collect(),
        },
        "write_to_file" if enriched => ToolRequestType::ModifyFile {
            file_path: arg_str(args, "TargetFile").unwrap_or_default(),
            // agy has no "previous content" argument. A write that overwrites
            // reports `Overwrite: true` and nothing about what it replaced, so
            // the before side is empty and the card renders as a whole-file
            // addition, which is what the tool actually did from the model's
            // point of view.
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
        // Headless `agy` answers this itself — "A1: User Skipped" — and ends
        // the turn without ever showing anyone the question. Tyde's contract
        // wants exactly that turn boundary: the card stays open past idle and
        // the user answers it afterwards, which arrives as a tool response and
        // is relayed to `agy` as the next turn. Without this mapping the step
        // renders as an opaque blob carrying neither the question nor its
        // options, because the stream gives it no name and no arguments.
        "ask_question" => ToolRequestType::AskUserQuestion {
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
        multi_select: raw
            .get("is_multi_select")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// What `agy` answered a question with on the user's behalf.
///
/// The transcript records the result as `A1: User Skipped` lines under the
/// usual timestamp header.
pub fn answers_from_result(result: &str) -> Vec<String> {
    result
        .lines()
        .map(str::trim)
        .filter(|line| {
            line.starts_with('A')
                && line
                    .split_once(':')
                    .is_some_and(|(index, _)| index[1..].chars().all(|c| c.is_ascii_digit()))
        })
        .map(str::to_owned)
        .collect()
}

/// `agy` routes every MCP tool through this one dispatcher rather than
/// advertising the tools themselves: the `init` frame's tool list is the same
/// 57 built-ins whether or not MCP servers are configured.
pub const MCP_DISPATCH_TOOL: &str = "call_mcp_tool";

/// The tool name and arguments inside a `call_mcp_tool` dispatch.
///
/// A third-party tool is qualified with its server so two servers exposing the
/// same tool name stay distinguishable. Tools arriving through Tyde's own
/// bridge are not: they are already unique, Tyde owns the name, and qualifying
/// them would stop the agent-control matchers recognising a spawn — those match
/// the bare `tyde_spawn_agent`, and `tyde__tyde_spawn_agent` normalizes to
/// something none of their patterns accept.
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

/// The child agent a `subagent` step spawned.
pub fn subagent_request_type(subagent: &AgySubagent) -> ToolRequestType {
    ToolRequestType::AgentSpawn {
        prompt: subagent.initial_prompt.clone(),
        name: subagent.role.clone().or_else(|| subagent.type_name.clone()),
        // The parent turn stays open until the child answers: measured, the
        // subagent's reply arrived as a `system_message` step inside the same
        // turn and the turn's `result` did not land until after it.
        execution_mode: AgentExecutionMode::Foreground,
    }
}

// ── Capacity ────────────────────────────────────────────────────────────────

/// The payload `agy -p "/usage"` answers with.
///
/// It runs no turn and spends no quota — measured, `num_turns: 0` and an
/// all-zero usage object — so it is safe to poll on a cadence.
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
    /// `weekly` or `5h` on agy 1.1.20. Anything else is reported as unknown
    /// rather than guessed at.
    #[serde(default)]
    pub window: String,
    /// How much of the limit is left, not how much is used.
    #[serde(default)]
    pub remaining_fraction: Option<f64>,
    #[serde(default)]
    pub reset_time: Option<String>,
}

/// Pulls the `/usage` payload out of a `command_result` or `result` frame.
pub fn usage_report_from_frame(frame: &AgyFrame) -> Option<AgyUsageReport> {
    let command = match frame {
        AgyFrame::CommandResult(command) => command,
        _ => return None,
    };
    if command.get("name").and_then(Value::as_str) != Some("usage") {
        return None;
    }
    serde_json::from_value::<AgyUsageReport>(command.get("data")?.clone()).ok()
}
