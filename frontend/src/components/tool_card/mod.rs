//! Tool-call rendering.
//!
//! `ToolCardView` is the per-tool card mounted by the chat row. Body rendering
//! is dispatched by an exhaustive `match` on `ToolRequestType`, with one module
//! per variant. The `ToolOutputMode` signal (`Summary` / `Compact` / `Full`) is
//! a frontend-only viewing preference; it controls how much of the tool's
//! output the body shows. Each renderer decides what `Summary` means for its
//! variant (usually empty), what `Compact` shows under per-tool caps, and what
//! `Full` lays out without truncation.
//!
//! Errors are special-cased in the shell: any completed tool whose result is
//! `ToolExecutionResult::Error` routes through `error_result::render`, no
//! matter the request kind.

use std::sync::Arc;

use leptos::prelude::*;
use protocol::{ToolExecutionResult, ToolRequestType};
use wasm_bindgen::JsCast;

use crate::state::{AppState, StreamingToolRequest, ToolOutputMode, ToolRequestEntry};

mod ask_user_question;
mod error_result;
mod exit_plan_mode;
mod get_type_docs;
mod modify_file;
mod other;
mod read_files;
mod run_command;
mod search_types;

const TOOL_LIST_INLINE_LIMIT: usize = 80;
const TOOL_LIST_HEAD_COUNT: usize = 8;
const TOOL_LIST_TAIL_COUNT: usize = 32;

#[cfg(all(test, target_arch = "wasm32"))]
pub(crate) mod test_utils;

#[component]
pub fn ToolCardListView(entries: Vec<ToolRequestEntry>) -> impl IntoView {
    let entries = Arc::new(entries);
    let expanded = RwSignal::new(false);
    let total = entries.len();

    view! {
        <div class="chat-card-tools">
            <For
                each={
                    let entries = entries.clone();
                    move || {
                        let expanded = expanded.get();
                        visible_tool_indexes(entries.len(), expanded, |idx| {
                            is_important_tool(&entries[idx])
                        })
                        .into_iter()
                        .map(|idx| entries[idx].clone())
                        .collect::<Vec<_>>()
                    }
                }
                key=|entry| entry.request.tool_call_id.clone()
                let:entry
            >
                <ToolCardView entry=entry />
            </For>
            <ToolListSummary
                total=move || total
                hidden_count={
                    let entries = entries.clone();
                    move || {
                        let visible = visible_tool_indexes(entries.len(), expanded.get(), |idx| {
                            is_important_tool(&entries[idx])
                        })
                        .len();
                        entries.len().saturating_sub(visible)
                    }
                }
                expanded=expanded
            />
        </div>
    }
}

#[component]
pub fn StreamingToolCardListView(entries: ArcRwSignal<Vec<StreamingToolRequest>>) -> impl IntoView {
    let expanded = RwSignal::new(false);

    view! {
        <div class="chat-card-tools">
            <For
                each={
                    let entries = entries.clone();
                    move || {
                        let expanded = expanded.get();
                        entries.with(|tools| {
                            visible_tool_indexes(tools.len(), expanded, |idx| {
                                tools[idx].entry.with_untracked(is_important_tool)
                            })
                            .into_iter()
                            .map(|idx| tools[idx].clone())
                            .collect::<Vec<_>>()
                        })
                    }
                }
                key=|tool| tool.tool_call_id.clone()
                let:tool
            >
                <StreamingToolCardView entry=tool.entry />
            </For>
            <ToolListSummary
                total={
                    let entries = entries.clone();
                    move || entries.with(|tools| tools.len())
                }
                hidden_count={
                    let entries = entries.clone();
                    move || {
                        entries.with(|tools| {
                            let visible = visible_tool_indexes(tools.len(), expanded.get(), |idx| {
                                tools[idx].entry.with_untracked(is_important_tool)
                            })
                            .len();
                            tools.len().saturating_sub(visible)
                        })
                    }
                }
                expanded=expanded
            />
        </div>
    }
}

#[component]
fn StreamingToolCardView(entry: ArcRwSignal<ToolRequestEntry>) -> impl IntoView {
    view! {
        {move || view! { <ToolCardView entry=entry.get() /> }}
    }
}

#[component]
fn ToolListSummary(
    total: impl Fn() -> usize + Send + Sync + 'static,
    hidden_count: impl Fn() -> usize + Send + Sync + 'static,
    expanded: RwSignal<bool>,
) -> impl IntoView {
    view! {
        {move || {
            let total = total();
            if total <= TOOL_LIST_INLINE_LIMIT {
                None
            } else {
                let is_expanded = expanded.get();
                let label = if is_expanded {
                    format!("{total} tools")
                } else {
                    format!("{} tools hidden", hidden_count())
                };
                let button_label = if is_expanded { "Show fewer" } else { "Show all" };
                Some(view! {
                    <div class="tool-list-summary">
                        <span class="tool-list-hidden-count">{label}</span>
                        <button
                            type="button"
                            class="tool-list-expand"
                            on:click=move |_| expanded.update(|value| *value = !*value)
                        >
                            {button_label}
                        </button>
                    </div>
                })
            }
        }}
    }
}

fn visible_tool_indexes<F>(len: usize, expanded: bool, mut is_important: F) -> Vec<usize>
where
    F: FnMut(usize) -> bool,
{
    if expanded || len <= TOOL_LIST_INLINE_LIMIT {
        return (0..len).collect();
    }

    let mut visible = vec![false; len];
    for keep in visible.iter_mut().take(TOOL_LIST_HEAD_COUNT) {
        *keep = true;
    }
    for keep in visible
        .iter_mut()
        .skip(len.saturating_sub(TOOL_LIST_TAIL_COUNT))
    {
        *keep = true;
    }
    for (idx, keep) in visible.iter_mut().enumerate() {
        if is_important(idx) {
            *keep = true;
        }
    }

    visible
        .into_iter()
        .enumerate()
        .filter_map(|(idx, keep)| keep.then_some(idx))
        .collect()
}

fn is_important_tool(entry: &ToolRequestEntry) -> bool {
    // Approval-gated tools (questions, plan approval) stay visible even in
    // collapsed long lists so a successful decision remains discoverable.
    matches!(
        &entry.request.tool_type,
        ToolRequestType::AskUserQuestion { .. } | ToolRequestType::ExitPlanMode { .. }
    ) || entry.result.as_ref().is_none_or(|result| !result.success)
}

#[component]
pub fn ToolCardView(entry: ToolRequestEntry) -> impl IntoView {
    let state = expect_context::<AppState>();
    let tool_output_mode = state.tool_output_mode;

    let tool_name = entry.request.tool_name.clone();
    let tool_call_id = entry.request.tool_call_id.clone();
    let tool_type = entry.request.tool_type;
    let result = entry.result;

    let has_result = result.is_some();
    let result_success = result.as_ref().map(|r| r.success).unwrap_or(false);
    let result_failed = has_result && !result_success;

    let status_class = if !has_result {
        "tool-status-text pending"
    } else if result_success {
        "tool-status-text success"
    } else {
        "tool-status-text failure"
    };

    let status_label = if !has_result {
        "Running\u{2026}".to_owned()
    } else if result_success {
        "Done".to_owned()
    } else {
        "Failed".to_owned()
    };

    let (icon, header_detail) = tool_icon_and_detail(&tool_name, &tool_type);

    let completion_summary = result
        .as_ref()
        .map(|r| completion_header_summary(&tool_type, &r.tool_result));

    let is_ask_user_question = matches!(tool_type, ToolRequestType::AskUserQuestion { .. });
    let body_tool_type = tool_type.clone();
    let body_result = result.as_ref().map(|r| r.tool_result.clone());
    let body_tool_type_slot = StoredValue::new_local(body_tool_type);
    let body_result_slot = StoredValue::new_local(body_result);
    let tool_call_id_slot = StoredValue::new_local(tool_call_id);
    let details_open = RwSignal::new(is_ask_user_question || !has_result || !result_success);
    let default_open_for_body = move || {
        is_ask_user_question
            || !has_result
            || !result_success
            || tool_output_mode.get() != ToolOutputMode::Summary
    };
    let default_open_for_prop = move || {
        is_ask_user_question
            || !has_result
            || !result_success
            || tool_output_mode.get() != ToolOutputMode::Summary
    };
    let render_body_when = move || default_open_for_body() || details_open.get();

    view! {
        <details
            class="tool-card"
            prop:open=default_open_for_prop
            on:toggle=move |ev: leptos::ev::Event| {
                if let Some(target) = ev.target()
                    && let Ok(el) = target.dyn_into::<web_sys::HtmlDetailsElement>()
                {
                    details_open.set(el.open());
                }
            }
        >
            <summary class="tool-card-header">
                <span class="tool-card-icon">{icon}</span>
                <span class="tool-card-name">{tool_name}</span>
                {header_detail.map(|d| view! {
                    <span class="tool-card-detail">{d}</span>
                })}
                {completion_summary.map(|s| view! {
                    <span class="tool-completion-summary">{s}</span>
                })}
                <span class=status_class>{status_label}</span>
                <span class="tool-chevron">"\u{25b6}"</span>
            </summary>
            <Show when=render_body_when>
                <div class="tool-card-body">
                    {move || {
                        let mode = tool_output_mode.get();
                        tool_call_id_slot.with_value(|tool_call_id| {
                            body_tool_type_slot.with_value(|body_tool_type| {
                                body_result_slot.with_value(|body_result| {
                                    render_body(
                                        tool_call_id,
                                        body_tool_type,
                                        body_result.as_ref(),
                                        mode,
                                        result_failed,
                                    )
                                })
                            })
                        })
                    }}
                </div>
            </Show>
        </details>
    }
}

/// Dispatch table from request kind → renderer module. The compiler enforces
/// exhaustiveness here: adding a new `ToolRequestType` variant fails the build
/// until a renderer is wired in.
///
/// Errors short-circuit the dispatch — any completed tool whose result is
/// `Error` renders via `error_result`, regardless of which request kind issued
/// it.
fn render_body(
    tool_call_id: &str,
    req: &ToolRequestType,
    result: Option<&ToolExecutionResult>,
    mode: ToolOutputMode,
    result_failed: bool,
) -> AnyView {
    if let Some(ToolExecutionResult::Error { .. }) = result {
        return error_result::render(result.unwrap(), mode).into_any();
    }

    match req {
        ToolRequestType::ModifyFile { .. } => modify_file::render(req, result, mode).into_any(),
        ToolRequestType::RunCommand { .. } => run_command::render(req, result, mode).into_any(),
        ToolRequestType::ReadFiles { .. } => read_files::render(req, result, mode).into_any(),
        ToolRequestType::SearchTypes { .. } => search_types::render(req, result, mode).into_any(),
        ToolRequestType::GetTypeDocs { .. } => get_type_docs::render(req, result, mode).into_any(),
        // A failed completion for a question is no longer answerable: render the
        // raw result instead of the interactive card, mirroring the mobile tool
        // card. The realistic failure carries `ToolExecutionResult::Error`, which
        // the shell short-circuits above; this arm covers a non-`Error` result
        // that still reports `success=false`.
        ToolRequestType::AskUserQuestion { .. } if result_failed => {
            failed_result_body(result).into_any()
        }
        ToolRequestType::AskUserQuestion { .. } => {
            ask_user_question::render(req, result, mode).into_any()
        }
        ToolRequestType::ExitPlanMode { .. } => {
            exit_plan_mode::render(tool_call_id, req, result, mode).into_any()
        }
        ToolRequestType::Other { .. } => other::render(req, result, mode).into_any(),
    }
}

/// Body for a tool whose request kind has an interactive renderer (currently
/// only `AskUserQuestion`) but whose completion failed, so the interactive UI
/// must not show. `Error` results are handled by the shell's short-circuit; this
/// renders any other non-success result as raw JSON.
fn failed_result_body(result: Option<&ToolExecutionResult>) -> AnyView {
    let Some(result) = result else {
        return ().into_any();
    };
    let pretty = serde_json::to_string_pretty(result).unwrap_or_else(|_| format!("{result:?}"));
    view! { <pre class="tool-raw-result">{pretty}</pre> }.into_any()
}

// ── Header bits ─────────────────────────────────────────────────────────

fn tool_icon_and_detail(name: &str, tool_type: &ToolRequestType) -> (&'static str, Option<String>) {
    match tool_type {
        ToolRequestType::ModifyFile { file_path, .. } => ("\u{270f}", Some(short_path(file_path))),
        ToolRequestType::RunCommand { command, .. } => {
            let short = if command.len() > 60 {
                format!("{}\u{2026}", &command[..57])
            } else {
                command.clone()
            };
            ("\u{25b6}", Some(short))
        }
        ToolRequestType::ReadFiles { file_paths } => {
            let label = if file_paths.len() == 1 {
                short_path(&file_paths[0])
            } else {
                format!("{} files", file_paths.len())
            };
            ("\u{1f4c4}", Some(label))
        }
        ToolRequestType::SearchTypes { type_name, .. } => ("\u{1f50d}", Some(type_name.clone())),
        ToolRequestType::GetTypeDocs { type_path, .. } => ("\u{1f4d6}", Some(type_path.clone())),
        ToolRequestType::AskUserQuestion { questions } => {
            let detail = match questions.len() {
                0 => None,
                1 => questions[0].header.clone(),
                n => Some(format!("{n} questions")),
            };
            ("\u{2753}", detail)
        }
        ToolRequestType::ExitPlanMode { plan_path, .. } => (
            "\u{1f4dd}",
            plan_path.clone().or_else(|| Some("Plan".to_owned())),
        ),
        ToolRequestType::Other { .. } => {
            let icon = match name {
                n if n.contains("spawn") || n.contains("agent") => "\u{1f916}",
                n if n.contains("ask") || n.contains("question") || n.contains("input") => {
                    "\u{2753}"
                }
                _ => "\u{2699}",
            };
            (icon, None)
        }
    }
}

/// Short header line attached next to the status — legacy parity with
/// `completionHeaderDetail` in tools.ts. Renderer modules don't use this; the
/// shell does.
pub(crate) fn completion_header_summary(
    req: &ToolRequestType,
    result: &ToolExecutionResult,
) -> String {
    match result {
        ToolExecutionResult::ModifyFile {
            lines_added,
            lines_removed,
        } => format!("+{lines_added} -{lines_removed}"),
        ToolExecutionResult::RunCommand {
            exit_code,
            stdout,
            stderr,
        } => {
            let mut parts = Vec::with_capacity(3);
            parts.push(format!("exit {exit_code}"));
            let stdout_lines = count_summary_lines(stdout);
            let stderr_lines = count_summary_lines(stderr);
            if stdout_lines > 0 {
                parts.push(format!("out {stdout_lines}L"));
            }
            if stderr_lines > 0 {
                parts.push(format!("err {stderr_lines}L"));
            }
            // Suppress request-side info — the request's command is already in
            // the header detail. Keep this concise.
            let _ = req;
            parts.join(" \u{b7} ")
        }
        ToolExecutionResult::ReadFiles { files } => {
            let total_bytes: u64 = files.iter().map(|f| f.bytes).sum();
            if files.len() == 1 {
                format_bytes(total_bytes)
            } else {
                format!("{} files \u{b7} {}", files.len(), format_bytes(total_bytes))
            }
        }
        ToolExecutionResult::SearchTypes { types } => {
            if types.is_empty() {
                "no matches".to_owned()
            } else {
                format!(
                    "{} matching {}",
                    types.len(),
                    if types.len() == 1 { "type" } else { "types" }
                )
            }
        }
        ToolExecutionResult::GetTypeDocs { documentation } => {
            let lines = count_summary_lines(documentation);
            if lines == 0 {
                "no documentation".to_owned()
            } else {
                format!("documentation \u{b7} {lines}L")
            }
        }
        ToolExecutionResult::Error { short_message, .. } => {
            let trimmed = short_message.replace('\n', " ");
            if trimmed.len() > 90 {
                format!("error \u{b7} {}\u{2026}", &trimmed[..87])
            } else {
                format!("error \u{b7} {trimmed}")
            }
        }
        ToolExecutionResult::Other { .. } => String::new(),
    }
}

// ── Shared helpers ──────────────────────────────────────────────────────

pub(crate) fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= 2 {
        path.to_owned()
    } else {
        format!("\u{2026}/{}", parts[parts.len() - 2..].join("/"))
    }
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

pub(crate) fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// Line count for completion summaries. Empty/whitespace-only strings count
/// as 0 lines — matches legacy `countSummaryLines`.
pub(crate) fn count_summary_lines(text: &str) -> usize {
    if text.trim().is_empty() {
        0
    } else {
        text.split('\n').count()
    }
}

#[cfg(test)]
mod tool_visibility_tests {
    use super::*;
    use protocol::{
        AskUserQuestion, AskUserQuestionOption, ToolExecutionCompletedData, ToolRequest,
    };
    use serde_json::json;

    fn completed_entry(idx: usize, tool_type: ToolRequestType) -> ToolRequestEntry {
        ToolRequestEntry {
            request: ToolRequest {
                tool_call_id: format!("toolu_{idx}"),
                tool_name: match &tool_type {
                    ToolRequestType::AskUserQuestion { .. } => "AskUserQuestion".to_owned(),
                    _ => "OtherTool".to_owned(),
                },
                tool_type,
            },
            result: Some(ToolExecutionCompletedData {
                tool_call_id: format!("toolu_{idx}"),
                tool_name: "OtherTool".to_owned(),
                tool_result: ToolExecutionResult::Other { result: json!({}) },
                success: true,
                error: None,
            }),
        }
    }

    fn completed_other_entry(idx: usize) -> ToolRequestEntry {
        completed_entry(idx, ToolRequestType::Other { args: json!({}) })
    }

    fn completed_ask_entry(idx: usize) -> ToolRequestEntry {
        completed_entry(
            idx,
            ToolRequestType::AskUserQuestion {
                questions: vec![AskUserQuestion {
                    id: None,
                    question: "Which language?".to_owned(),
                    header: Some("Language".to_owned()),
                    options: vec![AskUserQuestionOption {
                        label: "Rust".to_owned(),
                        description: None,
                    }],
                    multi_select: false,
                }],
            },
        )
    }

    fn completed_exit_plan_entry(idx: usize) -> ToolRequestEntry {
        completed_entry(
            idx,
            ToolRequestType::ExitPlanMode {
                plan: Some("Step 1".to_owned()),
                plan_path: Some("docs/plan.md".to_owned()),
            },
        )
    }

    #[test]
    fn collapsed_large_lists_keep_successful_ask_questions_visible() {
        let mut entries: Vec<_> = (0..100).map(completed_other_entry).collect();
        entries[40] = completed_ask_entry(40);

        let visible =
            visible_tool_indexes(entries.len(), false, |idx| is_important_tool(&entries[idx]));

        assert!(
            visible.contains(&40),
            "successful AskUserQuestion should remain visible in collapsed lists"
        );
        assert!(
            !visible.contains(&41),
            "nearby successful non-important tool should stay hidden"
        );
    }

    #[test]
    fn collapsed_large_lists_keep_successful_exit_plan_mode_visible() {
        let mut entries: Vec<_> = (0..100).map(completed_other_entry).collect();
        entries[40] = completed_exit_plan_entry(40);

        let visible =
            visible_tool_indexes(entries.len(), false, |idx| is_important_tool(&entries[idx]));

        assert!(
            visible.contains(&40),
            "successful ExitPlanMode should remain discoverable in collapsed lists"
        );
        assert!(
            !visible.contains(&41),
            "nearby successful non-important tool should stay hidden"
        );
    }
}

#[cfg(test)]
mod completion_summary_tests {
    use super::*;
    use protocol::FileInfo;

    fn req_run_command() -> ToolRequestType {
        ToolRequestType::RunCommand {
            command: "ls".to_owned(),
            working_directory: String::new(),
        }
    }

    #[test]
    fn modify_file_summary() {
        let req = ToolRequestType::ModifyFile {
            file_path: "x".into(),
            before: String::new(),
            after: String::new(),
        };
        let res = ToolExecutionResult::ModifyFile {
            lines_added: 5,
            lines_removed: 3,
        };
        assert_eq!(completion_header_summary(&req, &res), "+5 -3");
    }

    #[test]
    fn run_command_summary_includes_streams() {
        let res = ToolExecutionResult::RunCommand {
            exit_code: 0,
            stdout: "a\nb\nc".to_owned(),
            stderr: "err1".to_owned(),
        };
        assert_eq!(
            completion_header_summary(&req_run_command(), &res),
            "exit 0 \u{b7} out 3L \u{b7} err 1L"
        );
    }

    #[test]
    fn run_command_summary_no_streams_omits_them() {
        let res = ToolExecutionResult::RunCommand {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        };
        assert_eq!(
            completion_header_summary(&req_run_command(), &res),
            "exit 0"
        );
    }

    #[test]
    fn run_command_summary_nonzero_exit() {
        let res = ToolExecutionResult::RunCommand {
            exit_code: 2,
            stdout: String::new(),
            stderr: "boom".to_owned(),
        };
        assert_eq!(
            completion_header_summary(&req_run_command(), &res),
            "exit 2 \u{b7} err 1L"
        );
    }

    #[test]
    fn read_files_single_shows_bytes() {
        let req = ToolRequestType::ReadFiles {
            file_paths: vec!["a".into()],
        };
        let res = ToolExecutionResult::ReadFiles {
            files: vec![FileInfo {
                path: "a".into(),
                bytes: 1234,
            }],
        };
        assert_eq!(completion_header_summary(&req, &res), "1.2KB");
    }

    #[test]
    fn read_files_multi_shows_count_and_total() {
        let req = ToolRequestType::ReadFiles {
            file_paths: vec!["a".into(), "b".into()],
        };
        let res = ToolExecutionResult::ReadFiles {
            files: vec![
                FileInfo {
                    path: "a".into(),
                    bytes: 500,
                },
                FileInfo {
                    path: "b".into(),
                    bytes: 1500,
                },
            ],
        };
        assert_eq!(
            completion_header_summary(&req, &res),
            "2 files \u{b7} 2.0KB"
        );
    }

    #[test]
    fn search_types_zero() {
        let req = ToolRequestType::SearchTypes {
            language: "rust".into(),
            workspace_root: "/r".into(),
            type_name: "Foo".into(),
        };
        let res = ToolExecutionResult::SearchTypes { types: vec![] };
        assert_eq!(completion_header_summary(&req, &res), "no matches");
    }

    #[test]
    fn search_types_singular_vs_plural() {
        let req = ToolRequestType::SearchTypes {
            language: "rust".into(),
            workspace_root: "/r".into(),
            type_name: "Foo".into(),
        };
        let one = ToolExecutionResult::SearchTypes {
            types: vec!["A".into()],
        };
        assert_eq!(completion_header_summary(&req, &one), "1 matching type");
        let many = ToolExecutionResult::SearchTypes {
            types: vec!["A".into(), "B".into()],
        };
        assert_eq!(completion_header_summary(&req, &many), "2 matching types");
    }

    #[test]
    fn get_type_docs_summary() {
        let req = ToolRequestType::GetTypeDocs {
            language: "rust".into(),
            workspace_root: "/r".into(),
            type_path: "std::vec::Vec".into(),
        };
        let empty = ToolExecutionResult::GetTypeDocs {
            documentation: "  \n  ".into(),
        };
        assert_eq!(completion_header_summary(&req, &empty), "no documentation");
        let docs = ToolExecutionResult::GetTypeDocs {
            documentation: "line1\nline2\nline3".into(),
        };
        assert_eq!(
            completion_header_summary(&req, &docs),
            "documentation \u{b7} 3L"
        );
    }

    #[test]
    fn error_summary_truncates_long_messages() {
        let req = req_run_command();
        let res = ToolExecutionResult::Error {
            short_message: "x".repeat(100),
            detailed_message: String::new(),
        };
        let out = completion_header_summary(&req, &res);
        assert!(out.starts_with("error \u{b7} "));
        assert!(out.ends_with('\u{2026}'));
    }

    #[test]
    fn error_summary_short_passes_through() {
        let req = req_run_command();
        let res = ToolExecutionResult::Error {
            short_message: "boom".into(),
            detailed_message: String::new(),
        };
        assert_eq!(completion_header_summary(&req, &res), "error \u{b7} boom");
    }

    #[test]
    fn count_summary_lines_handles_blank_and_text() {
        assert_eq!(count_summary_lines(""), 0);
        assert_eq!(count_summary_lines("   "), 0);
        assert_eq!(count_summary_lines("\n\n"), 0);
        assert_eq!(count_summary_lines("a"), 1);
        assert_eq!(count_summary_lines("a\nb"), 2);
        assert_eq!(count_summary_lines("a\nb\n"), 3);
    }
}
