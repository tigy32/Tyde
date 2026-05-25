use leptos::prelude::*;

use crate::state::{AppState, ToolOutputMode, ToolRequestEntry};

/// Renders a single tool request inside an assistant message.
///
/// Carries semantic state through the `data-mobile-test` selector
/// (`tool-card-running`, `tool-card-success`, `tool-card-failed`) so
/// tests don't need to guess at color or icon. Failed and running
/// cards always reveal their output detail; successful cards honor
/// the global `ToolOutputMode`.
#[component]
pub fn ToolCardView(entry: ToolRequestEntry) -> impl IntoView {
    let state = expect_context::<AppState>();
    let tool_output_mode = state.tool_output_mode;

    let tool_name = entry.request.tool_name.clone();
    let is_completed = entry.result.is_some();
    let success = entry.result.as_ref().map(|r| r.success).unwrap_or(false);
    let result_summary = entry
        .result
        .as_ref()
        .map(|r| format!("{:?}", r.tool_result))
        .unwrap_or_default();

    let (status_class, status_icon, status_test, aria_label) = if is_completed {
        if success {
            (
                "completed success",
                "\u{2713}",
                "tool-card-success",
                "Tool completed successfully",
            )
        } else {
            (
                "completed failed",
                "\u{2717}",
                "tool-card-failed",
                "Tool failed",
            )
        }
    } else {
        (
            "running",
            "\u{25D4}",
            "tool-card-running",
            "Tool is running",
        )
    };

    // Failed/running cards always show their detail; otherwise honor the mode.
    let force_show = !is_completed || !success;

    view! {
        <div class=format!("tool-card {status_class}") data-mobile-test=status_test aria-label=aria_label>
            <div class="tool-card-header">
                <span class="tool-status-icon" aria-hidden="true">{status_icon}</span>
                <span class="tool-name">{tool_name}</span>
            </div>
            {
                let rs = result_summary.clone();
                let rs2 = result_summary.clone();
                let show = move || {
                    !rs.is_empty()
                        && (force_show || tool_output_mode.get() != ToolOutputMode::Summary)
                };
                view! {
                    <Show when=show>
                        <details class="tool-result" data-mobile-test="tool-card-result" prop:open=move || tool_output_mode.get() == ToolOutputMode::Full>
                            <summary>"Result"</summary>
                            <pre class="tool-output">{rs2.clone()}</pre>
                        </details>
                    </Show>
                }
            }
        </div>
    }
}
