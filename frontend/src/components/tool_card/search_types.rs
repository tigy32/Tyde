//! `SearchTypes` renderer.
//!
//! Request body shows the query + language. Result body shows matching type
//! names as inline code badges. Compact caps at 12 with a `+N more` line;
//! Full shows everything; Summary shows a one-line `Found N matching types`.

use leptos::prelude::*;
use protocol::{ToolExecutionResult, ToolRequestType};

use crate::state::ToolOutputMode;

const COMPACT_MAX_BADGES: usize = 12;

pub(crate) fn render(
    req: &ToolRequestType,
    result: Option<&ToolExecutionResult>,
    mode: ToolOutputMode,
) -> AnyView {
    let ToolRequestType::SearchTypes {
        type_name,
        language,
        workspace_root,
    } = req
    else {
        unreachable!("search_types::render dispatched on non-SearchTypes request");
    };

    let request_view = render_request(type_name, language, workspace_root, mode);
    let result_view = match result {
        Some(ToolExecutionResult::SearchTypes { types }) => Some(render_result(types, mode)),
        Some(_) | None => None,
    };

    view! {
        <div class="tool-result-search">
            {request_view}
            {result_view}
        </div>
    }
    .into_any()
}

fn render_request(
    type_name: &str,
    language: &str,
    _workspace_root: &str,
    mode: ToolOutputMode,
) -> Option<impl IntoView> {
    if mode == ToolOutputMode::Summary {
        return None;
    }
    let q = type_name.to_owned();
    let lang = language.to_owned();
    Some(view! {
        <div class="tool-request-detail">
            <code class="tool-search-query">{q}</code>
            <span class="tool-lang-badge">{lang}</span>
        </div>
    })
}

fn render_result(types: &[String], mode: ToolOutputMode) -> impl IntoView {
    let total = types.len();

    match mode {
        ToolOutputMode::Summary => {
            let label = if total == 0 {
                "No matching types".to_owned()
            } else if total == 1 {
                "Found 1 matching type".to_owned()
            } else {
                format!("Found {total} matching types")
            };
            view! { <div class="tool-meta-line">{label}</div> }.into_any()
        }
        ToolOutputMode::Compact | ToolOutputMode::Full => {
            if total == 0 {
                return view! {
                    <div class="tool-meta-line">"No matching types"</div>
                }
                .into_any();
            }

            let visible: Vec<String> = if mode == ToolOutputMode::Compact {
                types.iter().take(COMPACT_MAX_BADGES).cloned().collect()
            } else {
                types.to_vec()
            };
            let hidden = total.saturating_sub(visible.len());
            let has_hidden = hidden > 0;

            view! {
                <div class="tool-result-search-badges">
                    {visible.into_iter().map(|t| view! {
                        <code class="tool-result-type-badge">{t}</code>
                    }).collect::<Vec<_>>()}
                </div>
                <Show when=move || has_hidden>
                    <div class="tool-meta-line">{format!("+{hidden} more")}</div>
                </Show>
            }
            .into_any()
        }
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::components::tool_card::test_utils::*;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    fn req() -> ToolRequestType {
        ToolRequestType::SearchTypes {
            language: "rust".to_owned(),
            workspace_root: "/r".to_owned(),
            type_name: "Foo".to_owned(),
        }
    }

    fn result(n: usize) -> ToolExecutionResult {
        ToolExecutionResult::SearchTypes {
            types: (0..n).map(|i| format!("Type{i}")).collect(),
        }
    }

    #[wasm_bindgen_test]
    async fn summary_shows_count_line_only() {
        let r = result(5);
        let container = mount(move || render(&req(), Some(&r), ToolOutputMode::Summary));
        next_tick().await;
        let body = text(&container);
        assert!(body.contains("Found 5"));
        assert_eq!(count(&container, ".tool-result-type-badge"), 0);
    }

    #[wasm_bindgen_test]
    async fn summary_zero_says_no_matches() {
        let r = result(0);
        let container = mount(move || render(&req(), Some(&r), ToolOutputMode::Summary));
        next_tick().await;
        assert!(text(&container).contains("No matching types"));
    }

    #[wasm_bindgen_test]
    async fn compact_caps_at_twelve_with_more_line() {
        let r = result(20);
        let container = mount(move || render(&req(), Some(&r), ToolOutputMode::Compact));
        next_tick().await;
        assert_eq!(
            count(&container, ".tool-result-type-badge"),
            COMPACT_MAX_BADGES
        );
        assert!(text(&container).contains("+8 more"));
    }

    #[wasm_bindgen_test]
    async fn full_shows_all_badges() {
        let r = result(20);
        let container = mount(move || render(&req(), Some(&r), ToolOutputMode::Full));
        next_tick().await;
        assert_eq!(count(&container, ".tool-result-type-badge"), 20);
    }
}
