//! Generic `Other` renderer — covers tool variants not yet promoted to typed
//! `ToolRequestType` variants (spawn, AskUserQuestion, plan modes, grep, …).
//!
//! Summary: `Result JSON · KB`. Compact: pretty JSON capped at 30 lines with
//! a "Show more" toggle. Full: full pretty JSON.
//!
//! **Normalization failure.** The server marks canonical request normalization
//! failure on the paired completion and preserves the rejected request as
//! `Other`. This renderer makes a sanitized copy inspectable instead of dropping
//! the mixed request/result shape in Summary mode.
//!
//! So when that pairing is observed, a sanitized request is surfaced in **every**
//! mode behind a closed disclosure, with the drift announced. This is an error
//! state, not normal output: it is selected by the typed normalization marker,
//! not inferred from a tool name or error prose, and it leaves well-formed `Other` tools
//! (spawn included) byte-for-byte unchanged.

use leptos::prelude::*;
use protocol::{ToolExecutionResult, ToolRequestType};

use crate::state::ToolOutputMode;

use super::format_bytes;

const COMPACT_LINE_CAP: usize = 30;

pub(crate) fn render(
    req: &ToolRequestType,
    result: Option<&ToolExecutionResult>,
    malformed_payload: Option<&serde_json::Value>,
    mode: ToolOutputMode,
) -> AnyView {
    let ToolRequestType::Other { args } = req else {
        unreachable!("other::render dispatched on non-Other request");
    };

    if let Some(payload) = malformed_payload {
        log::error!(
            "tool card: request normalization drift reached an untyped request. A \
             sanitized payload is surfaced in the card."
        );
        return render_drift(payload).into_any();
    }

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

/// The malformed-payload body: an announced error plus a sanitized copy of the
/// request that failed to normalize, reachable in every output mode.
///
/// The raw payload sits behind a **closed** native disclosure rather than an open
/// `<pre>`, so surfacing it costs one line of chrome instead of reintroducing the
/// JSON blob this whole change removed. It is available in `Summary` because a
/// malformed payload is a failure the operator must be able to diagnose — output
/// mode governs how much *normal* output to show, not whether a failure is
/// inspectable.
pub(super) fn render_drift(args: &serde_json::Value) -> impl IntoView {
    let pretty = super::sanitized_request_payload_json(args);

    view! {
        <div class="tool-result-other">
            <div class="tool-typed-mismatch" role="alert">
                "This tool call could not be normalized. A sanitized copy of the \
                 request payload is available below."
            </div>
            <details class="tool-malformed-payload">
                <summary class="tool-result-section-title">"Sanitized raw request"</summary>
                <pre class="tool-raw-args">{pretty}</pre>
            </details>
        </div>
    }
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
    use wasm_bindgen::JsCast;
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
        let container = mount(move || render(&req(), Some(&r), None, ToolOutputMode::Summary));
        next_tick().await;
        let body = text(&container);
        assert!(body.contains("Result JSON"));
        assert_eq!(count(&container, "pre.tool-raw-result"), 0);
    }

    #[wasm_bindgen_test]
    async fn compact_under_cap_no_toggle() {
        let r = small_result();
        let container = mount(move || render(&req(), Some(&r), None, ToolOutputMode::Compact));
        next_tick().await;
        assert_eq!(count(&container, "pre.tool-raw-result"), 1);
        assert!(!has_show_more(&container));
    }

    #[wasm_bindgen_test]
    async fn compact_over_cap_truncates_with_toggle() {
        let r = big_result();
        let container = mount(move || render(&req(), Some(&r), None, ToolOutputMode::Compact));
        next_tick().await;
        assert!(has_show_more(&container));
    }

    // ── Malformed canonical payload (untyped request + typed result) ─────

    /// The malformed canonical pairing plus secret-like fields that must be
    /// redacted before the request becomes inspectable.
    fn malformed_canonical_req() -> ToolRequestType {
        ToolRequestType::Other {
            args: json!({
                "tool": "mcp__tyde-agent-control__tyde_send_agent_message",
                "arguments": {
                    "agent_id": "",
                    "message": "",
                    "api_key": "must-not-render",
                    "x-api-key": "x-must-not-render",
                    "OPENAI_API_KEY": "openai-must-not-render",
                    "authorization_header": "auth-must-not-render",
                    "bearer": "bearer-must-not-render",
                    "nested": [{ "github_token": "github-must-not-render" }],
                    "input": "{\"client_secret\":\"embedded-must-not-render\",\"safe\":\"kept\"}",
                },
                "array": [{ "access_token": "also-must-not-render" }],
            }),
        }
    }

    fn malformed_args(request: &ToolRequestType) -> &serde_json::Value {
        let ToolRequestType::Other { args } = request else {
            unreachable!();
        };
        args
    }

    /// Regression lock for QA D1. The payload that failed to normalize must be
    /// inspectable — in **every** mode, `Summary` included. The whole point of the
    /// server's fallback to `Other` is that the offending request stays visible; the
    /// renderer used to drop them and leave an empty card body.
    #[wasm_bindgen_test]
    async fn malformed_payload_is_inspectable_in_every_mode() {
        for mode in [
            ToolOutputMode::Summary,
            ToolOutputMode::Compact,
            ToolOutputMode::Full,
        ] {
            let result = ToolExecutionResult::TydeSendAgentMessage;
            let request = malformed_canonical_req();
            let container = mount(move || {
                render(
                    &request,
                    Some(&result),
                    Some(malformed_args(&request)),
                    mode,
                )
            });
            next_tick().await;

            let body = text(&container);
            assert!(
                body.contains("could not be normalized"),
                "the drift is announced in {mode:?}: {body}"
            );

            let details = container
                .query_selector("details.tool-malformed-payload")
                .expect("query disclosure")
                .unwrap_or_else(|| panic!("raw payload is reachable in {mode:?}"))
                .dyn_into::<web_sys::HtmlDetailsElement>()
                .expect("details element");
            assert!(
                !details.open(),
                "it stays closed by default in {mode:?} — inspectable, not a JSON blob"
            );

            let raw = details.text_content().unwrap_or_default();
            assert!(
                raw.contains("agent_id") && raw.contains("tyde_send_agent_message"),
                "the disclosure carries the useful payload fields in {mode:?}: {raw}"
            );
            assert!(raw.contains("[REDACTED]") && raw.contains("kept"));
            for secret in [
                "must-not-render",
                "x-must-not-render",
                "openai-must-not-render",
                "auth-must-not-render",
                "bearer-must-not-render",
                "github-must-not-render",
                "embedded-must-not-render",
                "also-must-not-render",
            ] {
                assert!(
                    !raw.contains(secret),
                    "secret reached desktop DOM in {mode:?}: {secret}: {raw}"
                );
            }
        }
    }

    /// The drift note is announced to assistive tech, not merely drawn.
    #[wasm_bindgen_test]
    async fn malformed_payload_drift_is_announced() {
        let result = ToolExecutionResult::TydeSendAgentMessage;
        let request = malformed_canonical_req();
        let container = mount(move || {
            render(
                &request,
                Some(&result),
                Some(malformed_args(&request)),
                ToolOutputMode::Summary,
            )
        });
        next_tick().await;

        let alert = container
            .query_selector(".tool-typed-mismatch")
            .expect("query alert")
            .expect("drift note present");
        assert_eq!(alert.get_attribute("role").as_deref(), Some("alert"));
    }

    #[wasm_bindgen_test]
    async fn malformed_payload_markup_is_rendered_only_as_text() {
        let request = ToolRequestType::Other {
            args: json!({
                "tool": "mcp__tyde-agent-control__tyde_send_agent_message",
                "arguments": {
                    "agent_id": "",
                    "message": "<img src=x onerror=alert(1)><script>alert(2)</script>",
                },
            }),
        };
        let result = ToolExecutionResult::TydeSendAgentMessage;
        let container = mount(move || {
            render(
                &request,
                Some(&result),
                Some(malformed_args(&request)),
                ToolOutputMode::Summary,
            )
        });
        next_tick().await;

        assert_eq!(count(&container, "script"), 0);
        assert_eq!(count(&container, "img"), 0);
        let raw = container
            .query_selector("pre.tool-raw-args")
            .expect("query raw request")
            .expect("raw request present")
            .text_content()
            .unwrap_or_default();
        assert!(
            raw.contains("<img") && raw.contains("<script>"),
            "markup remains inspectable as inert text: {raw}"
        );
    }

    /// A well-formed generic tool is untouched: no drift note, and `Summary` still
    /// shows no raw payload. This is the guard against the fix leaking JSON blobs
    /// back into normal cards — spawn, grep, and every other `Other` tool included.
    #[wasm_bindgen_test]
    async fn well_formed_other_tool_is_unchanged() {
        for mode in [
            ToolOutputMode::Summary,
            ToolOutputMode::Compact,
            ToolOutputMode::Full,
        ] {
            let result = small_result();
            let container = mount(move || render(&req(), Some(&result), None, mode));
            next_tick().await;

            assert_eq!(
                count(&container, "details.tool-malformed-payload"),
                0,
                "no drift disclosure on a well-formed tool in {mode:?}"
            );
            assert_eq!(
                count(&container, ".tool-typed-mismatch"),
                0,
                "no drift note on a well-formed tool in {mode:?}"
            );
        }

        // And Summary still shows no raw request payload at all.
        let result = small_result();
        let container = mount(move || render(&req(), Some(&result), None, ToolOutputMode::Summary));
        next_tick().await;
        assert_eq!(count(&container, "pre.tool-raw-args"), 0);
    }
}
