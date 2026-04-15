use leptos::prelude::*;
use protocol::{ToolExecutionResult, ToolRequestType};

use crate::state::ToolRequestEntry;

#[component]
pub fn ToolCardView(entry: ToolRequestEntry) -> impl IntoView {
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

    // Build completion summary for the header line (e.g. "+5 -3", "exit 0", "2 files · 25KB")
    let completion_summary = result
        .as_ref()
        .map(|r| completion_header_summary(&r.tool_result));

    let request_view = render_tool_request(tool_type);
    let result_view = result.map(|r| render_tool_result(r.tool_result));

    // Default: open if running (no result), collapsed if done successfully, open if failed
    let default_open = !has_result || !result_success;

    view! {
        <details class="tool-card" open=default_open>
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
                <span class="tool-chevron">"▶"</span>
            </summary>
            <div class="tool-card-body">
                {request_view}
                {result_view}
            </div>
        </details>
    }
}

/// Build a short completion summary for the header, like the legacy app does.
fn completion_header_summary(result: &ToolExecutionResult) -> String {
    match result {
        ToolExecutionResult::ModifyFile {
            lines_added,
            lines_removed,
        } => format!("+{lines_added} -{lines_removed}"),
        ToolExecutionResult::RunCommand { exit_code, .. } => format!("exit {exit_code}"),
        ToolExecutionResult::ReadFiles { files } => {
            let total_bytes: u64 = files.iter().map(|f| f.bytes).sum();
            if files.len() == 1 {
                format_bytes(total_bytes)
            } else {
                format!("{} files \u{b7} {}", files.len(), format_bytes(total_bytes))
            }
        }
        ToolExecutionResult::SearchTypes { types } => {
            format!("{} types", types.len())
        }
        ToolExecutionResult::GetTypeDocs { documentation } => {
            let lines = documentation.lines().count();
            format!("{lines} lines")
        }
        ToolExecutionResult::Error { short_message, .. } => {
            if short_message.len() > 40 {
                format!("{}\u{2026}", &short_message[..37])
            } else {
                short_message.clone()
            }
        }
        ToolExecutionResult::Other { .. } => String::new(),
    }
}

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

fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= 2 {
        path.to_owned()
    } else {
        format!("\u{2026}/{}", parts[parts.len() - 2..].join("/"))
    }
}

fn render_tool_request(tool_type: ToolRequestType) -> impl IntoView {
    match tool_type {
        ToolRequestType::ModifyFile {
            file_path,
            before,
            after,
        } => {
            let diff_html = render_inline_diff(&before, &after);
            view! {
                <div class="tool-request-detail">
                    <div class="tool-file-path">{file_path}</div>
                    <div class="tool-inline-diff" inner_html=diff_html></div>
                </div>
            }
            .into_any()
        }
        ToolRequestType::RunCommand {
            command,
            working_directory,
        } => {
            let cwd_check = working_directory.clone();
            let cwd_display = working_directory;
            view! {
                <div class="tool-request-detail">
                    <code class="tool-command-line">{command}</code>
                    <Show when=move || !cwd_check.is_empty()>
                        <span class="tool-cwd">{cwd_display.clone()}</span>
                    </Show>
                </div>
            }
            .into_any()
        }
        ToolRequestType::ReadFiles { file_paths } => view! {
            <div class="tool-request-detail">
                <div class="tool-read-file-list">
                    {file_paths.into_iter().map(|p| {
                        let display = short_path(&p);
                        view! {
                            <div class="tool-read-file-row">
                                <span class="tool-read-file-icon">"\u{1f4c4}"</span>
                                <span class="tool-read-file-path">{display}</span>
                            </div>
                        }
                    }).collect::<Vec<_>>()}
                </div>
            </div>
        }
        .into_any(),
        ToolRequestType::SearchTypes {
            type_name,
            language,
            ..
        } => view! {
            <div class="tool-request-detail">
                <code class="tool-search-query">{type_name}</code>
                <span class="tool-lang-badge">{language}</span>
            </div>
        }
        .into_any(),
        ToolRequestType::GetTypeDocs {
            type_path,
            language,
            ..
        } => view! {
            <div class="tool-request-detail">
                <code class="tool-search-query">{type_path}</code>
                <span class="tool-lang-badge">{language}</span>
            </div>
        }
        .into_any(),
        ToolRequestType::Other { args } => {
            let text = match serde_json::to_string_pretty(&args) {
                Ok(s) => s,
                Err(e) => format!("[serialization error: {e}]"),
            };
            view! {
                <div class="tool-request-detail">
                    <pre class="tool-raw-args">{text}</pre>
                </div>
            }
            .into_any()
        }
    }
}

fn render_tool_result(result: ToolExecutionResult) -> impl IntoView {
    match result {
        ToolExecutionResult::ModifyFile {
            lines_added,
            lines_removed,
        } => view! {
            <div class="tool-result-modify">
                <span class="tool-lines-added">{format!("+{lines_added}")}</span>
                <span class="tool-lines-removed">{format!("-{lines_removed}")}</span>
            </div>
        }
        .into_any(),
        ToolExecutionResult::RunCommand {
            exit_code,
            stdout,
            stderr,
        } => {
            let stdout_check = stdout.clone();
            let stderr_check = stderr.clone();
            let exit_class = if exit_code == 0 {
                "tool-exit-code exit-success"
            } else {
                "tool-exit-code exit-failure"
            };
            view! {
                <div class="tool-result-command">
                    <span class=exit_class>{format!("exit {exit_code}")}</span>
                    <Show when=move || !stdout_check.is_empty()>
                        <pre class="tool-result-stdout">{stdout.clone()}</pre>
                    </Show>
                    <Show when=move || !stderr_check.is_empty()>
                        <pre class="tool-result-stderr">{stderr.clone()}</pre>
                    </Show>
                </div>
            }
        }
        .into_any(),
        ToolExecutionResult::ReadFiles { files } => view! {
            <div class="tool-result-read">
                {files.into_iter().map(|f| {
                    let display = short_path(&f.path);
                    let size = format_bytes(f.bytes);
                    view! {
                        <div class="tool-result-file">
                            <span class="tool-result-file-icon">"\u{1f4c4}"</span>
                            <span class="tool-result-file-path">{display}</span>
                            <span class="tool-result-file-sep">"\u{b7}"</span>
                            <span class="tool-result-file-size">{size}</span>
                        </div>
                    }
                }).collect::<Vec<_>>()}
            </div>
        }
        .into_any(),
        ToolExecutionResult::SearchTypes { types } => view! {
            <div class="tool-result-search">
                {types.into_iter().map(|t| view! {
                    <code class="tool-result-type-badge">{t}</code>
                }).collect::<Vec<_>>()}
            </div>
        }
        .into_any(),
        ToolExecutionResult::GetTypeDocs { documentation } => view! {
            <details class="tool-result-docs">
                <summary class="tool-result-docs-summary">"Documentation"</summary>
                <pre class="tool-result-docs-content">{documentation}</pre>
            </details>
        }
        .into_any(),
        ToolExecutionResult::Error {
            short_message,
            detailed_message,
        } => {
            let detail_check = detailed_message.clone();
            view! {
                <div class="tool-result-error">
                    <span class="tool-error-icon">"\u{2715}"</span>
                    <span class="tool-error-short">{short_message}</span>
                    <Show when=move || !detail_check.is_empty()>
                        <details class="tool-error-details">
                            <summary>"Details"</summary>
                            <pre class="tool-error-detail">{detailed_message.clone()}</pre>
                        </details>
                    </Show>
                </div>
            }
        }
        .into_any(),
        ToolExecutionResult::Other { result } => {
            let text = match serde_json::to_string_pretty(&result) {
                Ok(s) => s,
                Err(e) => format!("[serialization error: {e}]"),
            };
            view! {
                <div class="tool-result-other">
                    <pre class="tool-raw-result">{text}</pre>
                </div>
            }
            .into_any()
        }
    }
}

fn render_inline_diff(before: &str, after: &str) -> String {
    let mut html = String::new();
    html.push_str("<div class=\"inline-diff-code\">");

    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();

    for line in &before_lines {
        html.push_str(&format!(
            "<div class=\"diff-line-removed\"><span class=\"diff-prefix\">-</span><span class=\"diff-text\">{}</span></div>",
            escape_html(line)
        ));
    }
    for line in &after_lines {
        html.push_str(&format!(
            "<div class=\"diff-line-added\"><span class=\"diff-prefix\">+</span><span class=\"diff-text\">{}</span></div>",
            escape_html(line)
        ));
    }

    html.push_str("</div>");
    html
}

fn escape_html(s: &str) -> String {
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

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}
