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

use leptos::prelude::*;
use protocol::{ToolExecutionResult, ToolRequestType};

use crate::state::{AppState, ToolOutputMode, ToolRequestEntry};

mod error_result;
mod get_type_docs;
mod modify_file;
mod other;
mod read_files;
mod run_command;
mod search_types;

#[cfg(all(test, target_arch = "wasm32"))]
pub(crate) mod test_utils;

#[component]
pub fn ToolCardView(entry: ToolRequestEntry) -> impl IntoView {
    let state = expect_context::<AppState>();
    let tool_output_mode = state.tool_output_mode;

    let tool_name = entry.request.tool_name.clone();
    let tool_type = entry.request.tool_type;
    let result = entry.result;

    let has_result = result.is_some();
    let result_success = result.as_ref().map(|r| r.success).unwrap_or(false);

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

    // Body is reactive on tool_output_mode so the user can flip the global
    // toggle and every card re-lays out without remounting.
    let body_tool_type = tool_type.clone();
    let body_result = result.as_ref().map(|r| r.tool_result.clone());
    let body = move || {
        let mode = tool_output_mode.get();
        render_body(&body_tool_type, body_result.as_ref(), mode)
    };

    view! {
        <details class="tool-card" open=move || {
            // Failed tools always open. In-flight always open. Otherwise:
            // open in Compact/Full, collapsed in Summary.
            !has_result || !result_success || tool_output_mode.get() != ToolOutputMode::Summary
        }>
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
            <div class="tool-card-body">
                {body}
            </div>
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
    req: &ToolRequestType,
    result: Option<&ToolExecutionResult>,
    mode: ToolOutputMode,
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
        ToolRequestType::Other { .. } => other::render(req, result, mode).into_any(),
    }
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
