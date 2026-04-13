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
        "tool-card-status pending"
    } else if result_success {
        "tool-card-status success"
    } else {
        "tool-card-status failed"
    };

    let status_label = if !has_result {
        "Running..."
    } else if result_success {
        "Done"
    } else {
        "Failed"
    };

    view! {
        <details class="tool-card" open>
            <summary class="tool-card-header">
                <span class="tool-card-name">{tool_name}</span>
                <span class=status_class>{status_label}</span>
            </summary>
            <div class="tool-card-body">
                {render_tool_type(tool_type)}
                {result.map(|r| render_tool_result(r.tool_result))}
            </div>
        </details>
    }
}

fn render_tool_type(tool_type: ToolRequestType) -> impl IntoView {
    match tool_type {
        ToolRequestType::ModifyFile {
            file_path,
            before,
            after,
        } => view! {
            <div class="tool-detail">
                <div class="tool-file-path">{file_path}</div>
                <div class="tool-diff">
                    <pre class="tool-diff-before">{before}</pre>
                    <pre class="tool-diff-after">{after}</pre>
                </div>
            </div>
        }
        .into_any(),
        ToolRequestType::RunCommand {
            command,
            working_directory,
        } => view! {
            <div class="tool-detail">
                <code class="tool-command">{command}</code>
                <span class="tool-cwd">{working_directory}</span>
            </div>
        }
        .into_any(),
        ToolRequestType::ReadFiles { file_paths } => view! {
            <div class="tool-detail">
                <ul class="tool-file-list">
                    {file_paths.into_iter().map(|p| view! { <li>{p}</li> }).collect::<Vec<_>>()}
                </ul>
            </div>
        }
        .into_any(),
        ToolRequestType::SearchTypes {
            type_name,
            language,
            ..
        } => view! {
            <div class="tool-detail">
                <span class="tool-label">"Search: "</span>
                <code>{type_name}</code>
                <span class="tool-label">" ("</span>{language}<span class="tool-label">")"</span>
            </div>
        }
        .into_any(),
        ToolRequestType::GetTypeDocs {
            type_path,
            language,
            ..
        } => view! {
            <div class="tool-detail">
                <span class="tool-label">"Docs: "</span>
                <code>{type_path}</code>
                <span class="tool-label">" ("</span>{language}<span class="tool-label">")"</span>
            </div>
        }
        .into_any(),
        ToolRequestType::Other { args } => {
            let text = match serde_json::to_string_pretty(&args) {
                Ok(s) => s,
                Err(e) => format!("[serialization error: {e}]"),
            };
            view! {
                <div class="tool-detail">
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
            <div class="tool-result">
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
            view! {
                <div class="tool-result">
                    <span class="tool-exit-code">{format!("exit {exit_code}")}</span>
                    <Show when=move || !stdout_check.is_empty()>
                        <pre class="tool-stdout">{stdout.clone()}</pre>
                    </Show>
                    <Show when=move || !stderr_check.is_empty()>
                        <pre class="tool-stderr">{stderr.clone()}</pre>
                    </Show>
                </div>
            }
        }
        .into_any(),
        ToolExecutionResult::ReadFiles { files } => view! {
            <div class="tool-result">
                {files.into_iter().map(|f| view! {
                    <div class="tool-file-info">
                        <span>{f.path}</span>
                        <span class="tool-file-size">{format_bytes(f.bytes)}</span>
                    </div>
                }).collect::<Vec<_>>()}
            </div>
        }
        .into_any(),
        ToolExecutionResult::SearchTypes { types } => view! {
            <div class="tool-result">
                <ul class="tool-type-list">
                    {types.into_iter().map(|t| view! { <li><code>{t}</code></li> }).collect::<Vec<_>>()}
                </ul>
            </div>
        }
        .into_any(),
        ToolExecutionResult::GetTypeDocs { documentation } => view! {
            <div class="tool-result">
                <pre class="tool-docs">{documentation}</pre>
            </div>
        }
        .into_any(),
        ToolExecutionResult::Error {
            short_message,
            detailed_message,
        } => {
            let detail_check = detailed_message.clone();
            view! {
                <div class="tool-result tool-result-error">
                    <span class="tool-error-short">{short_message}</span>
                    <Show when=move || !detail_check.is_empty()>
                        <pre class="tool-error-detail">{detailed_message.clone()}</pre>
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
                <div class="tool-result">
                    <pre class="tool-raw-result">{text}</pre>
                </div>
            }
            .into_any()
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}
