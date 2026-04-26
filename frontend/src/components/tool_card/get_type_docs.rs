//! `GetTypeDocs` renderer.
//!
//! Request body shows the requested type path + language. Result body shows
//! the documentation in a pre-block. Compact caps at 30 lines with a "Show
//! more" toggle; Full shows everything; Summary shows nothing (header detail
//! covers line count).

use leptos::prelude::*;
use protocol::{ToolExecutionResult, ToolRequestType};

use crate::state::ToolOutputMode;

const COMPACT_LINE_CAP: usize = 30;

pub(crate) fn render(
    req: &ToolRequestType,
    result: Option<&ToolExecutionResult>,
    mode: ToolOutputMode,
) -> AnyView {
    let ToolRequestType::GetTypeDocs {
        type_path,
        language,
        workspace_root,
    } = req
    else {
        unreachable!("get_type_docs::render dispatched on non-GetTypeDocs request");
    };

    let request_view = render_request(type_path, language, workspace_root, mode);
    let result_view = match result {
        Some(ToolExecutionResult::GetTypeDocs { documentation }) => {
            Some(render_result(documentation, mode))
        }
        Some(_) | None => None,
    };

    view! {
        <div class="tool-result-docs">
            {request_view}
            {result_view}
        </div>
    }
    .into_any()
}

fn render_request(
    type_path: &str,
    language: &str,
    _workspace_root: &str,
    mode: ToolOutputMode,
) -> Option<impl IntoView> {
    if mode == ToolOutputMode::Summary {
        return None;
    }
    let p = type_path.to_owned();
    let lang = language.to_owned();
    Some(view! {
        <div class="tool-request-detail">
            <code class="tool-search-query">{p}</code>
            <span class="tool-lang-badge">{lang}</span>
        </div>
    })
}

fn render_result(documentation: &str, mode: ToolOutputMode) -> impl IntoView {
    if mode == ToolOutputMode::Summary {
        return view! { <span></span> }.into_any();
    }
    if documentation.trim().is_empty() {
        return view! {
            <div class="tool-meta-line">"No documentation returned"</div>
        }
        .into_any();
    }

    let docs = documentation.to_owned();
    let line_count = docs.split('\n').count();
    let over_cap = mode == ToolOutputMode::Compact && line_count > COMPACT_LINE_CAP;
    let expanded = RwSignal::new(!over_cap);

    let display_docs = {
        let docs = docs.clone();
        move || {
            if expanded.get() {
                docs.clone()
            } else {
                let kept: Vec<&str> = docs.split('\n').take(COMPACT_LINE_CAP).collect();
                let mut out = kept.join("\n");
                out.push_str("\n\u{2026}");
                out
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
        <details class="tool-result-docs-section" open=true>
            <summary class="tool-result-docs-summary">"Documentation"</summary>
            <pre class="tool-result-docs-content">{display_docs}</pre>
            <Show when=move || over_cap>
                <button
                    class="tool-show-more"
                    on:click=move |_| expanded.update(|v| *v = !*v)
                >{toggle_label}</button>
            </Show>
        </details>
    }
    .into_any()
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::components::tool_card::test_utils::*;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    fn req() -> ToolRequestType {
        ToolRequestType::GetTypeDocs {
            language: "rust".to_owned(),
            workspace_root: "/r".to_owned(),
            type_path: "std::vec::Vec".to_owned(),
        }
    }

    fn docs(lines: usize) -> ToolExecutionResult {
        ToolExecutionResult::GetTypeDocs {
            documentation: (0..lines)
                .map(|i| format!("doc{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    #[wasm_bindgen_test]
    async fn summary_renders_no_pre_block() {
        let r = docs(10);
        let container = mount(move || render(&req(), Some(&r), ToolOutputMode::Summary));
        next_tick().await;
        assert_eq!(count(&container, ".tool-result-docs-content"), 0);
    }

    #[wasm_bindgen_test]
    async fn compact_under_cap_shows_full_docs_no_toggle() {
        let r = docs(10);
        let container = mount(move || render(&req(), Some(&r), ToolOutputMode::Compact));
        next_tick().await;
        assert!(text(&container).contains("doc9"));
        assert!(!has_show_more(&container));
    }

    #[wasm_bindgen_test]
    async fn compact_over_cap_truncates_with_toggle() {
        let r = docs(80);
        let container = mount(move || render(&req(), Some(&r), ToolOutputMode::Compact));
        next_tick().await;
        let body = text(&container);
        assert!(body.contains("doc0"));
        assert!(!body.contains("doc79"));
        assert!(has_show_more(&container));
    }
}
