//! Generic `Other` renderer — covers tool variants not yet promoted to typed
//! `ToolRequestType` variants (spawn, AskUserQuestion, plan modes, grep, …).
//!
//! Summary: `Result JSON · KB`. Compact: pretty JSON capped at 30 lines with
//! a "Show more" toggle. Full: full pretty JSON.

use leptos::prelude::*;
use protocol::{ToolExecutionResult, ToolRequestType};

use crate::state::ToolOutputMode;

use super::format_bytes;

const COMPACT_LINE_CAP: usize = 30;

pub(crate) fn render(
    req: &ToolRequestType,
    result: Option<&ToolExecutionResult>,
    mode: ToolOutputMode,
) -> AnyView {
    let ToolRequestType::Other { args } = req else {
        unreachable!("other::render dispatched on non-Other request");
    };

    let request_view = render_request(args, mode);
    let result_view = match result {
        Some(ToolExecutionResult::Other { result }) => Some(render_result(result, mode)),
        Some(_) | None => None,
    };

    view! {
        <div class="tool-result-other">
            {request_view}
            {result_view}
        </div>
    }
    .into_any()
}

fn render_request(args: &serde_json::Value, mode: ToolOutputMode) -> Option<impl IntoView> {
    if mode == ToolOutputMode::Summary {
        return None;
    }
    let pretty = serde_json::to_string_pretty(args).unwrap_or_else(|e| {
        log::warn!("failed to pretty-print Other tool args: {e}");
        args.to_string()
    });
    let line_count = pretty.split('\n').count();
    let over_cap = mode == ToolOutputMode::Compact && line_count > COMPACT_LINE_CAP;
    let expanded = RwSignal::new(!over_cap);

    let display = {
        let pretty = pretty.clone();
        move || {
            if expanded.get() {
                pretty.clone()
            } else {
                let kept: Vec<&str> = pretty.split('\n').take(COMPACT_LINE_CAP).collect();
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

    Some(view! {
        <div class="tool-request-detail">
            <pre class="tool-raw-args">{display}</pre>
            <Show when=move || over_cap>
                <button
                    class="tool-show-more"
                    on:click=move |_| expanded.update(|v| *v = !*v)
                >{toggle_label}</button>
            </Show>
        </div>
    })
}

fn render_result(result: &serde_json::Value, mode: ToolOutputMode) -> impl IntoView {
    let compact = serde_json::to_string(result).unwrap_or_else(|_| result.to_string());
    let pretty = serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string());

    if mode == ToolOutputMode::Summary {
        return view! {
            <div class="tool-meta-line">{format!("Result JSON \u{b7} {}", format_bytes(compact.len() as u64))}</div>
        }
        .into_any();
    }

    let line_count = pretty.split('\n').count();
    let over_cap = mode == ToolOutputMode::Compact && line_count > COMPACT_LINE_CAP;
    let expanded = RwSignal::new(!over_cap);

    let display = {
        let pretty = pretty.clone();
        move || {
            if expanded.get() {
                pretty.clone()
            } else {
                let kept: Vec<&str> = pretty.split('\n').take(COMPACT_LINE_CAP).collect();
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
        <details class="tool-result-other-section" open=true>
            <summary class="tool-result-section-title">"Result JSON"</summary>
            <pre class="tool-raw-result">{display}</pre>
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
    use serde_json::json;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    fn req() -> ToolRequestType {
        ToolRequestType::Other {
            args: json!({"name": "foo", "n": 3}),
        }
    }

    fn small_result() -> ToolExecutionResult {
        ToolExecutionResult::Other {
            result: json!({"ok": true}),
        }
    }

    fn big_result() -> ToolExecutionResult {
        let lines: Vec<serde_json::Value> = (0..60).map(|i| json!({"i": i})).collect();
        ToolExecutionResult::Other {
            result: json!({"items": lines}),
        }
    }

    #[wasm_bindgen_test]
    async fn summary_shows_size_meta_only() {
        let r = small_result();
        let container = mount(move || render(&req(), Some(&r), ToolOutputMode::Summary));
        next_tick().await;
        let body = text(&container);
        assert!(body.contains("Result JSON"));
        assert_eq!(count(&container, "pre.tool-raw-result"), 0);
    }

    #[wasm_bindgen_test]
    async fn compact_under_cap_no_toggle() {
        let r = small_result();
        let container = mount(move || render(&req(), Some(&r), ToolOutputMode::Compact));
        next_tick().await;
        assert_eq!(count(&container, "pre.tool-raw-result"), 1);
        assert!(!has_show_more(&container));
    }

    #[wasm_bindgen_test]
    async fn compact_over_cap_truncates_with_toggle() {
        let r = big_result();
        let container = mount(move || render(&req(), Some(&r), ToolOutputMode::Compact));
        next_tick().await;
        assert!(has_show_more(&container));
    }
}
