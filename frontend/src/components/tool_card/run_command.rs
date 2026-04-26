//! `RunCommand` renderer.
//!
//! Request body is the literal command line plus optional working directory.
//! Result body is exit code + stdout/stderr pre-blocks. In Compact mode, each
//! pre-block collapses past the cap with a "Show more" toggle; in Full it
//! lays out without truncation; in Summary it shows nothing.

use leptos::prelude::*;
use protocol::{ToolExecutionResult, ToolRequestType};

use crate::state::ToolOutputMode;

/// Cap per pre-block in Compact mode. Either threshold trips the collapse.
const COMPACT_LINE_CAP: usize = 200;
const COMPACT_BYTE_CAP: usize = 8 * 1024;

pub(crate) fn render(
    req: &ToolRequestType,
    result: Option<&ToolExecutionResult>,
    mode: ToolOutputMode,
) -> AnyView {
    let ToolRequestType::RunCommand {
        command,
        working_directory,
    } = req
    else {
        unreachable!("run_command::render dispatched on non-RunCommand request");
    };

    let request_view = render_request(command, working_directory, mode);
    let result_view = match result {
        Some(ToolExecutionResult::RunCommand {
            exit_code,
            stdout,
            stderr,
        }) => Some(render_result(*exit_code, stdout, stderr, mode)),
        Some(_) | None => None,
    };

    view! {
        <div class="tool-result-command">
            {request_view}
            {result_view}
        </div>
    }
    .into_any()
}

fn render_request(
    command: &str,
    working_directory: &str,
    mode: ToolOutputMode,
) -> Option<impl IntoView> {
    if mode == ToolOutputMode::Summary {
        return None;
    }
    let cmd = command.to_owned();
    let cwd_present = !working_directory.is_empty();
    let cwd = working_directory.to_owned();
    Some(view! {
        <div class="tool-request-detail">
            <code class="tool-command-line">{cmd}</code>
            <Show when=move || cwd_present>
                <span class="tool-cwd">{cwd.clone()}</span>
            </Show>
        </div>
    })
}

fn render_result(
    exit_code: i32,
    stdout: &str,
    stderr: &str,
    mode: ToolOutputMode,
) -> impl IntoView {
    if mode == ToolOutputMode::Summary {
        return view! {
            <span></span>
        }
        .into_any();
    }

    let exit_class = if exit_code == 0 {
        "tool-exit-code exit-success"
    } else {
        "tool-exit-code exit-failure"
    };

    let stdout_view = if stdout.is_empty() {
        None
    } else {
        Some(pre_block(stdout, "tool-result-stdout", mode))
    };
    let stderr_view = if stderr.is_empty() {
        None
    } else {
        Some(pre_block(stderr, "tool-result-stderr", mode))
    };

    view! {
        <div class="tool-result-command-content">
            <span class=exit_class>{format!("exit {exit_code}")}</span>
            {stdout_view}
            {stderr_view}
        </div>
    }
    .into_any()
}

/// Render a pre-block with a "Show more" toggle when the content exceeds the
/// Compact-mode cap. In `Full` the block is shown in full immediately.
fn pre_block(text: &str, class: &'static str, mode: ToolOutputMode) -> impl IntoView {
    let text = text.to_owned();
    let line_count = text.split('\n').count();
    let over_cap = mode == ToolOutputMode::Compact
        && (line_count > COMPACT_LINE_CAP || text.len() > COMPACT_BYTE_CAP);

    let expanded = RwSignal::new(!over_cap);

    let display_text = {
        let text = text.clone();
        move || {
            if expanded.get() {
                text.clone()
            } else {
                truncate_for_compact(&text)
            }
        }
    };

    let toggle_label = move || {
        if expanded.get() {
            "Show less".to_owned()
        } else {
            format!("Show more ({line_count} lines)")
        }
    };

    view! {
        <div class="tool-pre-block">
            <pre class=class>{display_text}</pre>
            <Show when=move || over_cap>
                <button
                    class="tool-show-more"
                    on:click=move |_| expanded.update(|v| *v = !*v)
                >{toggle_label}</button>
            </Show>
        </div>
    }
}

fn truncate_for_compact(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut taken: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    for line in lines.iter().take(COMPACT_LINE_CAP) {
        if bytes + line.len() + 1 > COMPACT_BYTE_CAP {
            break;
        }
        bytes += line.len() + 1;
        taken.push(line);
    }
    let mut out = taken.join("\n");
    if taken.len() < lines.len() {
        out.push_str("\n\u{2026}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_passes_through() {
        let out = truncate_for_compact("a\nb\nc");
        assert_eq!(out, "a\nb\nc");
    }

    #[test]
    fn truncate_caps_at_line_cap() {
        let lines: Vec<String> = (0..300).map(|i| format!("line{i}")).collect();
        let input = lines.join("\n");
        let out = truncate_for_compact(&input);
        let kept = out.split('\n').count();
        // 200 lines + the trailing ellipsis line.
        assert_eq!(kept, COMPACT_LINE_CAP + 1);
        assert!(out.ends_with('\u{2026}'));
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::components::tool_card::test_utils::*;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    fn req() -> ToolRequestType {
        ToolRequestType::RunCommand {
            command: "ls -la".to_owned(),
            working_directory: "/tmp".to_owned(),
        }
    }

    fn short_result() -> ToolExecutionResult {
        ToolExecutionResult::RunCommand {
            exit_code: 0,
            stdout: "alpha\nbravo\ncharlie".to_owned(),
            stderr: String::new(),
        }
    }

    fn over_cap_result() -> ToolExecutionResult {
        let stdout: String = (0..300)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        ToolExecutionResult::RunCommand {
            exit_code: 0,
            stdout,
            stderr: String::new(),
        }
    }

    #[wasm_bindgen_test]
    async fn summary_renders_no_command_or_output() {
        let r = short_result();
        let container = mount(move || render(&req(), Some(&r), ToolOutputMode::Summary));
        next_tick().await;
        let body = text(&container);
        assert!(!body.contains("ls -la"), "summary should hide command");
        assert!(!body.contains("alpha"), "summary should hide stdout");
        assert!(!has_show_more(&container));
    }

    #[wasm_bindgen_test]
    async fn compact_under_cap_shows_full_output_no_toggle() {
        let r = short_result();
        let container = mount(move || render(&req(), Some(&r), ToolOutputMode::Compact));
        next_tick().await;
        let body = text(&container);
        assert!(body.contains("alpha"));
        assert!(body.contains("charlie"));
        assert!(!has_show_more(&container), "no toggle when under cap");
    }

    #[wasm_bindgen_test]
    async fn compact_over_cap_truncates_with_toggle() {
        let r = over_cap_result();
        let container = mount(move || render(&req(), Some(&r), ToolOutputMode::Compact));
        next_tick().await;
        let body = text(&container);
        assert!(body.contains("line0"));
        assert!(!body.contains("line299"), "over-cap content must be hidden");
        assert!(has_show_more(&container));
    }

    #[wasm_bindgen_test]
    async fn full_renders_everything() {
        let r = over_cap_result();
        let container = mount(move || render(&req(), Some(&r), ToolOutputMode::Full));
        next_tick().await;
        let body = text(&container);
        assert!(body.contains("line0"));
        assert!(body.contains("line299"));
        assert!(!has_show_more(&container), "full mode never truncates");
    }
}
