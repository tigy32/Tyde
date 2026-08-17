//! Failed outcome renderer — special-cased in the shell so any failed tool,
//! regardless of request kind, lands here.
//!
//! Summary: one-line `Error: <short_message>`.
//! Compact / Full: short_message in a pre-block, plus a collapsible "Details"
//! section if `detailed_message` is non-empty.

use crate::state::ToolOutputMode;
use leptos::prelude::*;

pub(crate) fn render(
    message: &str,
    details: Option<&str>,
    malformed_payload: Option<&serde_json::Value>,
    mode: ToolOutputMode,
) -> AnyView {
    let short = message.to_owned();
    let detail = details.unwrap_or_default().to_owned();
    let has_detail = !detail.is_empty();

    if mode == ToolOutputMode::Summary {
        let oneliner = single_line(&short);
        return view! {
            <div>
                {malformed_payload.map(super::other::render_drift)}
                <div class="tool-result-error">
                    <span class="tool-error-icon">"\u{2715}"</span>
                    <span class="tool-error-short">{format!("Error: {oneliner}")}</span>
                </div>
            </div>
        }
        .into_any();
    }

    view! {
        <div>
            {malformed_payload.map(super::other::render_drift)}
            <div class="tool-result-error">
                <span class="tool-error-icon">"\u{2715}"</span>
                <pre class="tool-result-stderr tool-error-pre">{short}</pre>
                <Show when=move || has_detail>
                    <details class="tool-error-details">
                        <summary>"Details"</summary>
                        <pre class="tool-error-detail">{detail.clone()}</pre>
                    </details>
                </Show>
            </div>
        </div>
    }
    .into_any()
}

fn single_line(text: &str) -> String {
    let s: String = text
        .chars()
        .map(|c| if c == '\n' { ' ' } else { c })
        .collect();
    let trimmed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.len() > 160 {
        format!("{}\u{2026}", &trimmed[..157])
    } else {
        trimmed
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::components::tool_card::test_utils::*;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    async fn summary_shows_one_liner() {
        let container = mount(move || {
            render(
                "boom over\nmultiple\nlines",
                None,
                None,
                ToolOutputMode::Summary,
            )
        });
        next_tick().await;
        let body = text(&container);
        // Newlines collapsed into a single visible line.
        assert!(body.contains("Error: boom over multiple lines"));
        assert_eq!(count(&container, "pre"), 0);
    }

    #[wasm_bindgen_test]
    async fn compact_shows_pre_and_collapsed_details() {
        let container = mount(move || {
            render(
                "boom",
                Some("stack trace here"),
                None,
                ToolOutputMode::Compact,
            )
        });
        next_tick().await;
        assert_eq!(count(&container, "pre.tool-error-pre"), 1);
        // <details> is present but collapsed by default; its summary should be
        // visible regardless.
        assert!(text(&container).contains("Details"));
    }

    #[wasm_bindgen_test]
    async fn compact_no_details_when_detailed_message_empty() {
        let container = mount(move || render("boom", None, None, ToolOutputMode::Compact));
        next_tick().await;
        assert_eq!(count(&container, ".tool-error-details"), 0);
    }
}
