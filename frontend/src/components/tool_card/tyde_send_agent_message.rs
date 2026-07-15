//! Semantic renderer for `tyde_send_agent_message`.
//!
//! The tool's entire payload is a human-authored message delivered to a child
//! agent, and its result is a bare `{"ok": true}` ack. Rendering the MCP
//! envelope as JSON therefore printed the message twice — escaped, monospaced,
//! and clipped — while carrying nothing else of value. This card instead names
//! the recipient and renders the message with the same Markdown renderer the
//! chat uses, so a reader can answer "who was messaged, and what was said?" at a
//! glance.
//!
//! Disclosure: `Summary` and `Compact` carry no JSON at all. `Full` adds a
//! closed `Raw tool data` disclosure holding the canonical typed request — the
//! one genuine diagnostic, since it confirms the exact bytes that were
//! delivered and how they were escaped.

use leptos::prelude::*;
use protocol::{AgentId, ToolExecutionResult, ToolRequestType};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use crate::markdown::render_markdown;
use crate::state::{ActiveAgentRef, AppState, TabContent, ToolOutputMode};

use super::agent_display_name;

pub(crate) fn render(
    agent_ref: Signal<Option<ActiveAgentRef>>,
    tool_call_id: &str,
    req: &ToolRequestType,
    result: Option<&ToolExecutionResult>,
    mode: ToolOutputMode,
) -> AnyView {
    let ToolRequestType::TydeSendAgentMessage { agent_id, message } = req else {
        unreachable!("tyde_send_agent_message::render dispatched on a non-send request");
    };

    // The completion is an ack with no body. Anything else means the request was
    // typed but the result was not — protocol drift, which must be loud.
    let mismatch = match result {
        None | Some(ToolExecutionResult::TydeSendAgentMessage) => None,
        Some(other) => {
            log::error!(
                "tyde_send_agent_message completed with a non-ack result: {}",
                result_kind(other)
            );
            Some(format!(
                "Unexpected result shape for tyde_send_agent_message: {}. The message above is the request that was sent.",
                result_kind(other)
            ))
        }
    };

    let raw = (mode == ToolOutputMode::Full).then(|| match serde_json::to_string_pretty(req) {
        Ok(pretty) => pretty,
        Err(error) => {
            log::error!("tyde_send_agent_message: failed to serialize typed request: {error}");
            format!("failed to serialize typed request: {error}")
        }
    });

    view! {
        <SendAgentMessageCard
            agent_ref=agent_ref
            body_id=format!("tool-send-message-{tool_call_id}")
            agent_id=agent_id.clone()
            message=message.clone()
            mismatch=mismatch
            raw=raw
        />
    }
    .into_any()
}

fn result_kind(result: &ToolExecutionResult) -> &'static str {
    match result {
        ToolExecutionResult::ModifyFile { .. } => "ModifyFile",
        ToolExecutionResult::RunCommand { .. } => "RunCommand",
        ToolExecutionResult::ReadFiles { .. } => "ReadFiles",
        ToolExecutionResult::SearchTypes { .. } => "SearchTypes",
        ToolExecutionResult::GetTypeDocs { .. } => "GetTypeDocs",
        ToolExecutionResult::Error { .. } => "Error",
        ToolExecutionResult::TydeSendAgentMessage => "TydeSendAgentMessage",
        ToolExecutionResult::TydeAwaitAgents { .. } => "TydeAwaitAgents",
        ToolExecutionResult::Other { .. } => "Other",
    }
}

#[component]
fn SendAgentMessageCard(
    agent_ref: Signal<Option<ActiveAgentRef>>,
    body_id: String,
    agent_id: AgentId,
    message: String,
    mismatch: Option<String>,
    raw: Option<String>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    let display_name = Signal::derive({
        let state = state.clone();
        let agent_id = agent_id.clone();
        move || agent_display_name(&state, agent_ref.get(), &agent_id, None)
    });

    let expanded = RwSignal::new(false);
    // Whether the clamped body actually overflows. Measured from the rendered
    // DOM rather than guessed from the source length: a long Markdown source can
    // render short (and vice versa), and a "Show more" button that reveals
    // nothing is worse than none at all.
    let overflowing = RwSignal::new(false);
    let body_ref = NodeRef::<leptos::html::Div>::new();

    // Re-measure on every resize, not just at mount. The card narrows — Tyde has
    // draggable splitters, and the layout reflows at 720px — and a message that
    // fit at the old width can overflow at the new one. Measured once, the toggle
    // would never appear and `overflow: hidden` would silently eat the rest of
    // the message with no way to reveal it.
    //
    // The observer and its closure are !Send/!Sync (they wrap raw JS pointers),
    // so they park in thread-local storage behind a `Copy` handle that the Effect
    // and `on_cleanup` can both hold.
    type ObserverPair = Option<(web_sys::ResizeObserver, Closure<dyn FnMut(js_sys::Array)>)>;
    let observer_slot: StoredValue<ObserverPair, LocalStorage> = StoredValue::new_local(None);

    Effect::new(move |_| {
        let Some(body) = body_ref.get() else {
            return;
        };
        measure_overflow(&body, expanded, overflowing);

        let element: web_sys::Element = body.clone().unchecked_into();
        let callback = Closure::<dyn FnMut(js_sys::Array)>::new(move |_: js_sys::Array| {
            let Some(body) = body_ref.get_untracked() else {
                return;
            };
            measure_overflow(&body, expanded, overflowing);
        });
        if let Ok(observer) = web_sys::ResizeObserver::new(callback.as_ref().unchecked_ref()) {
            observer.observe(&element);
            observer_slot.update_value(|slot| *slot = Some((observer, callback)));
        }
    });

    on_cleanup(move || {
        observer_slot.update_value(|slot| {
            if let Some((observer, _callback)) = slot.take() {
                observer.disconnect();
            }
        });
    });

    let on_open = {
        let state = state.clone();
        let agent_id = agent_id.clone();
        move |_: web_sys::MouseEvent| {
            let Some(parent) = agent_ref.get_untracked() else {
                log::error!("Open agent clicked on a send-message card with no resolved agent");
                return;
            };
            state.open_tab(
                TabContent::chat_with_agent(ActiveAgentRef {
                    host_id: parent.host_id,
                    agent_id: agent_id.clone(),
                }),
                display_name.get_untracked(),
                true,
            );
        }
    };

    // `overflow: hidden` clips content visually but leaves it in the tab order,
    // so a keyboard user can land on a link — or one of the copy buttons that
    // `render_markdown` puts on every fenced block — that is invisible on screen
    // (WCAG 2.4.7). Expanding on focus keeps every focusable child reachable
    // *and* visible, rather than making the clipped region unreachable.
    let on_focus_in = move |_: web_sys::FocusEvent| {
        if !expanded.get_untracked() {
            expanded.set(true);
        }
    };

    let toggle_body_id = body_id.clone();

    view! {
        <div class="tool-send-message">
            <div class="tool-send-message-header">
                <span class="tool-send-message-label">"To"</span>
                <span class="tool-send-message-recipient">{move || display_name.get()}</span>
                <button type="button" class="tool-live-link" on:click=on_open>"Open agent"</button>
            </div>
            <div
                id=body_id
                class="tool-send-message-body tool-md"
                class:clamped=move || !expanded.get()
                node_ref=body_ref
                on:focusin=on_focus_in
                inner_html=render_markdown(&message)
            ></div>
            <Show when=move || overflowing.get()>
                <button
                    type="button"
                    class="tool-show-more"
                    aria-controls=toggle_body_id.clone()
                    aria-expanded=move || if expanded.get() { "true" } else { "false" }
                    on:click=move |_| expanded.update(|value| *value = !*value)
                >
                    {move || if expanded.get() { "Show less" } else { "Show more" }}
                </button>
            </Show>
            {mismatch.map(|message| view! {
                <div class="tool-typed-mismatch" role="alert">{message}</div>
            })}
            // Labeled for what it actually is. It carries the *typed* request —
            // the canonical thing the server produced and the UI rendered — not
            // the MCP envelope. Calling it "raw tool data" would promise bytes
            // it does not have.
            {raw.map(|raw| view! {
                <details class="tool-send-message-raw">
                    <summary class="tool-result-section-title">"Typed request"</summary>
                    <pre class="tool-raw-args">{raw}</pre>
                </details>
            })}
        </div>
    }
}

/// Measure the clamped body against its content. Only meaningful while clamped:
/// once expanded, `scroll_height == client_height`, so re-measuring would clear
/// the flag and take the "Show less" affordance away with it.
fn measure_overflow(
    body: &web_sys::HtmlDivElement,
    expanded: RwSignal<bool>,
    overflowing: RwSignal<bool>,
) {
    if expanded.get_untracked() {
        return;
    }
    overflowing.set(body.scroll_height() > body.client_height() + 1);
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::components::tool_card::test_utils::*;
    use crate::state::AgentInfo;
    use leptos::mount::mount_to;
    use protocol::{AgentOrigin, BackendKind, StreamPath};
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::{HtmlButtonElement, HtmlElement};

    wasm_bindgen_test_configure!(run_in_browser);

    /// The clamp is a real CSS rule, so geometry assertions on it need the real
    /// stylesheet. Injected once per test session, mirroring the other geometry
    /// suites in this crate.
    const PROD_STYLES: &str = include_str!("../../../styles.css");

    fn ensure_styles_loaded() {
        let document = web_sys::window().unwrap().document().unwrap();
        if document
            .get_element_by_id("test-prod-styles-send")
            .is_none()
        {
            let style = document.create_element("style").unwrap();
            style.set_id("test-prod-styles-send");
            style.set_text_content(Some(PROD_STYLES));
            document.head().unwrap().append_child(&style).unwrap();
        }
    }

    const MESSAGE: &str = "## Fixing exact rerun behavior\n\n\
        - start with `mock.rs:666-695`\n\
        - then check `StreamEndData::default()`\n\n\
        Reply when done.";

    fn parent_ref() -> ActiveAgentRef {
        ActiveAgentRef {
            host_id: "host-1".to_owned(),
            agent_id: AgentId("agent-parent".to_owned()),
        }
    }

    fn send_req(message: &str) -> ToolRequestType {
        ToolRequestType::TydeSendAgentMessage {
            agent_id: AgentId("f0f48002-841c-4c76-8eea-2ecbc97f7993".to_owned()),
            message: message.to_owned(),
        }
    }

    fn child_agent(name: &str) -> AgentInfo {
        AgentInfo {
            host_id: "host-1".to_owned(),
            agent_id: AgentId("f0f48002-841c-4c76-8eea-2ecbc97f7993".to_owned()),
            name: name.to_owned(),
            origin: AgentOrigin::AgentControl,
            backend_kind: BackendKind::Codex,
            workspace_roots: vec!["/tmp/work".to_owned()],
            project_id: None,
            parent_agent_id: Some(parent_ref().agent_id),
            session_id: None,
            custom_agent_id: None,
            workflow: None,
            created_at_ms: 1,
            instance_stream: StreamPath("/agents/child".to_owned()),
            started: true,
            fatal_error: None,
            activity_summary: Default::default(),
        }
    }

    fn mount_send_card(
        message: &str,
        result: Option<ToolExecutionResult>,
        mode: ToolOutputMode,
        setup: impl FnOnce(&AppState) + 'static,
    ) -> (HtmlElement, AppState) {
        ensure_styles_loaded();
        let state = AppState::new();
        setup(&state);
        let container = make_container();
        let mount_state = state.clone();
        let req = send_req(message);
        let handle = mount_to(container.clone(), move || {
            provide_context(mount_state);
            let agent_ref = Signal::derive(|| Some(parent_ref()));
            render(agent_ref, "toolu_send", &req, result.as_ref(), mode)
        });
        handle.forget();
        (container, state)
    }

    fn body_element(container: &HtmlElement) -> HtmlElement {
        container
            .query_selector(".tool-send-message-body")
            .expect("query body")
            .expect("message body present")
            .dyn_into::<HtmlElement>()
            .expect("html element")
    }

    fn show_more_button(container: &HtmlElement) -> Option<HtmlButtonElement> {
        container
            .query_selector(".tool-show-more")
            .expect("query toggle")
            .and_then(|node| node.dyn_into::<HtmlButtonElement>().ok())
    }

    /// Regression lock for the screenshot's first defect: the sent message must
    /// render as real Markdown, and the JSON envelope that used to carry it must
    /// be gone from the default view.
    #[wasm_bindgen_test]
    async fn renders_markdown_not_json() {
        let (container, _state) = mount_send_card(
            MESSAGE,
            Some(ToolExecutionResult::TydeSendAgentMessage),
            ToolOutputMode::Compact,
            |_| {},
        );
        next_tick().await;

        // Real semantic HTML — headings, list items, and inline code — which is
        // also what a screen reader announces instead of a wall of JSON.
        assert_eq!(count(&container, "h2"), 1, "heading renders as a heading");
        assert_eq!(count(&container, "li"), 2, "bullets render as list items");
        assert!(
            count(&container, "code") >= 1,
            "inline code renders as code"
        );

        let body = text(&container);
        assert!(
            body.contains("Fixing exact rerun behavior"),
            "message text visible: {body}"
        );
        assert!(
            !body.contains("agent_id"),
            "no raw JSON keys in the default view: {body}"
        );
        assert!(
            !body.contains("\\n"),
            "newlines render as line breaks, not escaped \\n: {body}"
        );
        assert_eq!(
            count(&container, "pre.tool-raw-args"),
            0,
            "no raw args block outside Full mode"
        );
        assert_eq!(
            count(&container, "pre.tool-raw-result"),
            0,
            "the ack result has no JSON panel"
        );
    }

    /// Summary is the tightest mode and must still be JSON-free while showing
    /// the message exactly once.
    #[wasm_bindgen_test]
    async fn summary_mode_shows_message_once_and_no_json() {
        let (container, _state) = mount_send_card(
            MESSAGE,
            Some(ToolExecutionResult::TydeSendAgentMessage),
            ToolOutputMode::Summary,
            |_| {},
        );
        next_tick().await;

        let body = text(&container);
        assert_eq!(
            body.matches("Fixing exact rerun behavior").count(),
            1,
            "message appears exactly once: {body}"
        );
        assert_eq!(count(&container, "pre.tool-raw-args"), 0);
        assert_eq!(count(&container, "pre.tool-raw-result"), 0);
    }

    /// The recipient is named from live server-owned agent state, and a later
    /// rename must re-render the card — the pure-projection rule.
    #[wasm_bindgen_test]
    async fn names_recipient_from_live_state() {
        let (container, state) = mount_send_card(
            MESSAGE,
            Some(ToolExecutionResult::TydeSendAgentMessage),
            ToolOutputMode::Compact,
            |state| {
                state
                    .agents
                    .update(|agents| agents.push(child_agent("Agent state bugs")));
            },
        );
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("Agent state bugs"),
            "human name is shown: {body}"
        );
        assert!(
            !body.contains("f0f48002"),
            "the raw uuid must not be the recipient label: {body}"
        );

        state.agents.update(|agents| {
            agents[0].name = "Renamed worker".to_owned();
        });
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("Renamed worker"),
            "rename re-renders the card: {body}"
        );
    }

    /// With no agent record yet, the id is shown rather than a fabricated name.
    #[wasm_bindgen_test]
    async fn unknown_recipient_shows_the_id() {
        let (container, _state) = mount_send_card(MESSAGE, None, ToolOutputMode::Compact, |_| {});
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("f0f48002-841c-4c76-8eea-2ecbc97f7993"),
            "unknown recipient falls back to the id, never an invented name: {body}"
        );
    }

    /// A long message is clamped by the rendered container's height, and the
    /// toggle reveals the rest. Geometry, not source length, is the contract.
    #[wasm_bindgen_test]
    async fn long_message_is_clamped_with_a_working_toggle() {
        let long = (0..80)
            .map(|i| format!("Line {i} of a very long orchestration brief."))
            .collect::<Vec<_>>()
            .join("\n\n");
        let (container, _state) = mount_send_card(
            &long,
            Some(ToolExecutionResult::TydeSendAgentMessage),
            ToolOutputMode::Compact,
            |_| {},
        );
        next_tick().await;

        let body = body_element(&container);
        let clamped_height = body.get_bounding_client_rect().height();
        let full_height = body.scroll_height() as f64;
        assert!(
            clamped_height < full_height,
            "long message is clamped: rendered {clamped_height} vs content {full_height}"
        );

        let toggle = show_more_button(&container).expect("overflowing message offers a toggle");
        assert_eq!(
            toggle.get_attribute("aria-expanded").as_deref(),
            Some("false"),
            "collapsed state is announced"
        );

        toggle.click();
        next_tick().await;

        let expanded_height = body_element(&container).get_bounding_client_rect().height();
        assert!(
            expanded_height > clamped_height,
            "toggle expands the message: {expanded_height} vs {clamped_height}"
        );
        assert_eq!(
            show_more_button(&container)
                .expect("toggle stays")
                .get_attribute("aria-expanded")
                .as_deref(),
            Some("true"),
            "expanded state is announced"
        );
    }

    /// Regression lock: the clamp is re-measured when the card's width changes,
    /// not only at mount. A message that fits at full width overflows once the
    /// card is narrowed (a dragged splitter, the 720px reflow). Measured once,
    /// no toggle would appear and `overflow: hidden` would silently swallow the
    /// rest of the message with no way to reach it — content loss, no affordance.
    #[wasm_bindgen_test]
    async fn narrowing_the_card_reveals_the_toggle() {
        // One long paragraph: a handful of lines at 800px, dozens at 90px.
        let message = "word ".repeat(120);
        let (container, _state) = mount_send_card(
            &message,
            Some(ToolExecutionResult::TydeSendAgentMessage),
            ToolOutputMode::Compact,
            |_| {},
        );
        next_tick().await;
        assert!(
            show_more_button(&container).is_none(),
            "the message fits at full width, so no toggle is offered"
        );

        container
            .set_attribute(
                "style",
                "position: absolute; top: 0; left: 0; width: 90px; height: 600px;",
            )
            .expect("narrow the card");
        // ResizeObserver delivers on the next frame; give it two turns.
        next_tick().await;
        next_tick().await;

        assert!(
            show_more_button(&container).is_some(),
            "narrowing must reveal the toggle rather than silently clipping the message"
        );
    }

    /// `overflow: hidden` clips content visually but leaves it in the tab order,
    /// so a keyboard user can land on a link — or a fenced block's copy button —
    /// that is invisible on screen (WCAG 2.4.7). Focus must bring it into view.
    #[wasm_bindgen_test]
    async fn focusing_clipped_content_expands_the_message() {
        let mut message = "A filler paragraph that pushes the link out of view.\n\n".repeat(30);
        message.push_str("[deep link](https://example.com/deep)\n");
        let (container, _state) = mount_send_card(
            &message,
            Some(ToolExecutionResult::TydeSendAgentMessage),
            ToolOutputMode::Compact,
            |_| {},
        );
        next_tick().await;

        let toggle = show_more_button(&container).expect("a long message clamps");
        assert_eq!(
            toggle.get_attribute("aria-expanded").as_deref(),
            Some("false"),
            "starts collapsed"
        );

        let link = container
            .query_selector(".tool-send-message-body a")
            .expect("query link")
            .expect("the clipped region contains a focusable link")
            .dyn_into::<HtmlElement>()
            .expect("html element");
        link.focus().expect("focus the clipped link");
        next_tick().await;

        assert_eq!(
            show_more_button(&container)
                .expect("toggle stays")
                .get_attribute("aria-expanded")
                .as_deref(),
            Some("true"),
            "focusing a clipped child expands the message so it is never focused while invisible"
        );
    }

    /// The toggle names the region it controls, so a screen-reader user knows
    /// what just expanded.
    #[wasm_bindgen_test]
    async fn toggle_is_associated_with_the_message_body() {
        let long = (0..80)
            .map(|i| format!("Line {i} of a very long orchestration brief."))
            .collect::<Vec<_>>()
            .join("\n\n");
        let (container, _state) = mount_send_card(
            &long,
            Some(ToolExecutionResult::TydeSendAgentMessage),
            ToolOutputMode::Compact,
            |_| {},
        );
        next_tick().await;

        let body_id = body_element(&container)
            .get_attribute("id")
            .expect("the message body is addressable");
        let controls = show_more_button(&container)
            .expect("toggle")
            .get_attribute("aria-controls")
            .expect("the toggle declares what it controls");
        assert_eq!(
            controls, body_id,
            "aria-controls points at the message body"
        );
    }

    /// A short message must not offer a toggle that reveals nothing.
    #[wasm_bindgen_test]
    async fn short_message_offers_no_toggle() {
        let (container, _state) = mount_send_card(
            "Ship it.",
            Some(ToolExecutionResult::TydeSendAgentMessage),
            ToolOutputMode::Compact,
            |_| {},
        );
        next_tick().await;

        assert!(
            show_more_button(&container).is_none(),
            "a message that fits needs no Show more"
        );
    }

    /// Full mode is the only place raw diagnostics appear, and they start closed
    /// so they never dominate the conversation.
    #[wasm_bindgen_test]
    async fn full_mode_exposes_closed_raw_details() {
        let (container, _state) = mount_send_card(
            MESSAGE,
            Some(ToolExecutionResult::TydeSendAgentMessage),
            ToolOutputMode::Full,
            |_| {},
        );
        next_tick().await;

        let details = container
            .query_selector("details.tool-send-message-raw")
            .expect("query raw details")
            .expect("Full mode exposes raw diagnostics")
            .dyn_into::<web_sys::HtmlDetailsElement>()
            .expect("details element");
        assert!(!details.open(), "raw diagnostics start closed");

        // The message itself is still the primary content, rendered as Markdown.
        assert_eq!(count(&container, "h2"), 1, "Markdown still renders in Full");

        details.set_open(true);
        next_tick().await;
        let raw = container
            .query_selector("pre.tool-raw-args")
            .expect("query raw")
            .expect("opening the disclosure reveals the typed request")
            .text_content()
            .unwrap_or_default();
        assert!(
            raw.contains("agent_id") && raw.contains("message"),
            "raw diagnostics carry the exact typed request: {raw}"
        );
    }

    /// Protocol drift (a typed request whose completion is untyped) is surfaced,
    /// never silently swallowed.
    #[wasm_bindgen_test]
    async fn unexpected_result_shape_is_surfaced() {
        let (container, _state) = mount_send_card(
            MESSAGE,
            Some(ToolExecutionResult::Other {
                result: serde_json::json!({"ok": true}),
            }),
            ToolOutputMode::Compact,
            |_| {},
        );
        next_tick().await;

        let body = text(&container);
        assert!(
            body.contains("Unexpected result shape"),
            "a mismatched completion is visible: {body}"
        );
    }
}
