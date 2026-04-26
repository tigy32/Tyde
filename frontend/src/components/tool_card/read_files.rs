//! `ReadFiles` renderer.
//!
//! Request body lists the requested paths. Result body lists the read files
//! with their sizes. Compact caps the visible rows at 8 with a `+N more`
//! footer; Full shows everything; Summary shows nothing (header detail
//! covers the count).

use leptos::prelude::*;
use protocol::{ToolExecutionResult, ToolRequestType};

use crate::state::ToolOutputMode;

use super::{format_bytes, short_path};

const COMPACT_MAX_ROWS: usize = 8;

pub(crate) fn render(
    req: &ToolRequestType,
    result: Option<&ToolExecutionResult>,
    mode: ToolOutputMode,
) -> AnyView {
    let ToolRequestType::ReadFiles { file_paths } = req else {
        unreachable!("read_files::render dispatched on non-ReadFiles request");
    };

    let request_view = render_request(file_paths, mode);
    let result_view = match result {
        Some(ToolExecutionResult::ReadFiles { files }) => Some(render_result(files, mode)),
        Some(_) | None => None,
    };

    view! {
        <div class="tool-result-read">
            {request_view}
            {result_view}
        </div>
    }
    .into_any()
}

fn render_request(file_paths: &[String], mode: ToolOutputMode) -> Option<impl IntoView> {
    // Only show request-side rows pre-completion. Once the result arrives we
    // render the richer file-with-size list instead.
    if mode == ToolOutputMode::Summary {
        return None;
    }
    if mode == ToolOutputMode::Compact && file_paths.len() == 1 {
        // Header detail already shows the single path.
        return None;
    }

    let visible: Vec<String> = match mode {
        ToolOutputMode::Compact => file_paths.iter().take(COMPACT_MAX_ROWS).cloned().collect(),
        ToolOutputMode::Full => file_paths.to_vec(),
        ToolOutputMode::Summary => return None,
    };
    let hidden = file_paths.len().saturating_sub(visible.len());
    let has_hidden = hidden > 0;

    Some(view! {
        <div class="tool-request-detail">
            <div class="tool-read-file-list">
                {visible.into_iter().map(|p| {
                    let display = short_path(&p);
                    view! {
                        <div class="tool-read-file-row">
                            <span class="tool-read-file-icon">"\u{1f4c4}"</span>
                            <span class="tool-read-file-path">{display}</span>
                        </div>
                    }
                }).collect::<Vec<_>>()}
            </div>
            <Show when=move || has_hidden>
                <div class="tool-meta-line">{format!("+{hidden} more")}</div>
            </Show>
        </div>
    })
}

fn render_result(files: &[protocol::FileInfo], mode: ToolOutputMode) -> impl IntoView {
    if mode == ToolOutputMode::Summary {
        return view! { <span></span> }.into_any();
    }
    if mode == ToolOutputMode::Compact && files.len() == 1 {
        // Header detail shows path + size already.
        return view! { <span></span> }.into_any();
    }

    let visible: Vec<protocol::FileInfo> = match mode {
        ToolOutputMode::Compact => files.iter().take(COMPACT_MAX_ROWS).cloned().collect(),
        ToolOutputMode::Full => files.to_vec(),
        ToolOutputMode::Summary => return view! { <span></span> }.into_any(),
    };
    let hidden = files.len().saturating_sub(visible.len());
    let has_hidden = hidden > 0;

    view! {
        <div class="tool-result-file-list">
            {visible.into_iter().map(|f| {
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
            <Show when=move || has_hidden>
                <div class="tool-meta-line">{format!("+{hidden} more")}</div>
            </Show>
        </div>
    }
    .into_any()
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::components::tool_card::test_utils::*;
    use protocol::FileInfo;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    fn req(n: usize) -> ToolRequestType {
        ToolRequestType::ReadFiles {
            file_paths: (0..n).map(|i| format!("/p/file{i}.rs")).collect(),
        }
    }

    fn result(n: usize) -> ToolExecutionResult {
        ToolExecutionResult::ReadFiles {
            files: (0..n)
                .map(|i| FileInfo {
                    path: format!("/p/file{i}.rs"),
                    bytes: 1024,
                })
                .collect(),
        }
    }

    #[wasm_bindgen_test]
    async fn summary_renders_no_rows() {
        let r = result(3);
        let container = mount(move || render(&req(3), Some(&r), ToolOutputMode::Summary));
        next_tick().await;
        assert_eq!(count(&container, ".tool-result-file"), 0);
    }

    #[wasm_bindgen_test]
    async fn compact_under_cap_shows_all_rows() {
        let r = result(3);
        let container = mount(move || render(&req(3), Some(&r), ToolOutputMode::Compact));
        next_tick().await;
        assert_eq!(count(&container, ".tool-result-file"), 3);
        assert!(!text(&container).contains("more"));
    }

    #[wasm_bindgen_test]
    async fn compact_over_cap_caps_with_more_line() {
        let r = result(20);
        let container = mount(move || render(&req(20), Some(&r), ToolOutputMode::Compact));
        next_tick().await;
        assert_eq!(count(&container, ".tool-result-file"), COMPACT_MAX_ROWS);
        assert!(text(&container).contains("+12 more"));
    }

    #[wasm_bindgen_test]
    async fn full_shows_all_rows() {
        let r = result(20);
        let container = mount(move || render(&req(20), Some(&r), ToolOutputMode::Full));
        next_tick().await;
        assert_eq!(count(&container, ".tool-result-file"), 20);
    }
}
