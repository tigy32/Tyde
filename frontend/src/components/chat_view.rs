use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use crate::components::chat_input::ChatInput;
use crate::components::chat_message::ChatMessageView;
use crate::components::chat_streaming::ChatStreamingView;
use crate::components::task_list::TaskListView;
use crate::state::{ActiveAgentRef, AppState, TransientEvent};

use protocol::BackendKind;

#[component]
pub fn ChatView(
    /// Per-instance binding to a chat — typically derived from a tab's
    /// `TabContent::Chat { agent_ref }` so each tab has its own view that
    /// stays mounted even when the tab is hidden via CSS. Passed as a Signal
    /// so the view tracks the rare in-place mutation where a "New Chat" tab's
    /// agent_ref upgrades from `None` to the spawned agent (see
    /// `dispatch.rs` agent-creation handling).
    agent_ref: Signal<Option<ActiveAgentRef>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    let has_agent = move || agent_ref.get().is_some();

    // Reactive identifier of the chat the row list belongs to. Combined with
    // `idx` it forms the keyed `<For>` row identity below: switching agents
    // changes every key (clean remount), appending a message preserves rows
    // 0..len() and only mounts the new tail row.
    let active_agent_id = move || agent_ref.get().map(|a| a.agent_id);

    let messages_len: Memo<usize> = Memo::new(move |_| match active_agent_id() {
        Some(id) => state
            .chat_messages
            .with(|m| m.get(&id).map(|v| v.len()).unwrap_or(0)),
        None => 0,
    });

    let row_keys = move || -> Vec<(protocol::AgentId, usize)> {
        let Some(id) = active_agent_id() else {
            return Vec::new();
        };
        let len = messages_len.get();
        (0..len).map(|i| (id.clone(), i)).collect()
    };

    let streaming = move || {
        let agent_id = agent_ref.get()?.agent_id;
        let map = state.streaming_text.get();
        map.get(&agent_id).cloned()
    };

    let task_list = move || {
        let agent_id = agent_ref.get()?.agent_id;
        let map = state.task_lists.get();
        map.get(&agent_id).cloned()
    };

    // Walk back from the latest message to find the most recent assistant
    // message that carries a context_breakdown. `ContextBreakdown` does not
    // implement `PartialEq`, so we use a derived Signal rather than a Memo.
    // Each read still walks the vec, but it's bounded by "messages up to the
    // most recent assistant turn" — typically a single iteration.
    let context_breakdown: Signal<Option<protocol::ContextBreakdown>> = Signal::derive(move || {
        let id = active_agent_id()?;
        state.chat_messages.with(|m| {
            let messages = m.get(&id)?;
            for entry in messages.iter().rev() {
                let is_assistant = matches!(
                    entry.message.sender,
                    protocol::MessageSender::Assistant { .. }
                );
                if !is_assistant {
                    continue;
                }
                if let Some(breakdown) = entry.message.context_breakdown.clone() {
                    return Some(breakdown);
                }
                if entry.message.tool_calls.is_empty() {
                    return None;
                }
            }
            None
        })
    });

    let transient_events = move || {
        let agent_id = agent_ref.get()?.agent_id;
        let map = state.transient_events.get();
        map.get(&agent_id).cloned()
    };

    let agent_name = move || -> String {
        let Some(active_agent) = agent_ref.get() else {
            return String::new();
        };
        let agents = state.agents.get();
        match agents
            .iter()
            .find(|a| a.host_id == active_agent.host_id && a.agent_id == active_agent.agent_id)
        {
            Some(a) => a.name.clone(),
            None => "[unknown agent]".to_owned(),
        }
    };

    let agent_backend = move || -> Option<BackendKind> {
        let active_agent = agent_ref.get()?;
        let agents = state.agents.get();
        agents
            .iter()
            .find(|a| a.host_id == active_agent.host_id && a.agent_id == active_agent.agent_id)
            .map(|a| a.backend_kind)
    };

    let agent_initializing = move || -> bool {
        let active_agent = match agent_ref.get() {
            Some(active_agent) => active_agent,
            None => return false,
        };
        state.agents.get().iter().any(|agent| {
            agent.host_id == active_agent.host_id
                && agent.agent_id == active_agent.agent_id
                && !agent.started
                && agent.fatal_error.is_none()
        })
    };

    let scroll_ref = NodeRef::<leptos::html::Div>::new();
    let user_scrolled_up = RwSignal::new(false);
    let show_scroll_btn = RwSignal::new(false);

    // Per-instance scroll listener. Multiple `ChatView`s may exist
    // simultaneously (one per chat tab, mounted-and-hidden), so we cannot use
    // a thread-local single-slot handle. We forget the Closure so it survives
    // for the lifetime of the underlying scroll element; when the component
    // unmounts, the DOM element is removed and the listener is collected with
    // it. One leaked Closure per chat-tab mount is bounded and acceptable.
    let scroll_ref_for_handler = scroll_ref;
    Effect::new(move |_| {
        if let Some(el) = scroll_ref_for_handler.get() {
            let el_clone = el.clone();
            let handler = Closure::<dyn Fn()>::new(move || {
                let scroll_height = el_clone.scroll_height();
                let scroll_top = el_clone.scroll_top();
                let client_height = el_clone.client_height();
                let distance_from_bottom = scroll_height - scroll_top - client_height;
                let is_near_bottom = distance_from_bottom < 80;
                user_scrolled_up.set(!is_near_bottom);
                show_scroll_btn.set(!is_near_bottom);
            });
            let _ = el.add_event_listener_with_callback("scroll", handler.as_ref().unchecked_ref());
            handler.forget();
        }
    });

    // Auto-scroll effect: whenever the message count or streaming text grows,
    // scroll to bottom (only if the user hasn't scrolled up). Scoped to the
    // *length* of messages — not the full Vec — so unrelated chat_messages
    // updates (e.g. tool_request mutations to existing rows) don't trigger a
    // scroll.
    Effect::new(move |_| {
        let _len = messages_len.get();
        let stream = streaming();
        if let Some(ss) = stream.as_ref() {
            let _ = ss.text.get();
            let _ = ss.reasoning.get();
        }
        if user_scrolled_up.get_untracked() {
            return;
        }
        if let Some(el) = scroll_ref.get() {
            request_animation_frame(move || {
                el.set_scroll_top(el.scroll_height());
            });
        }
    });

    let scroll_to_bottom = move |_| {
        if let Some(el) = scroll_ref.get() {
            el.set_scroll_top(el.scroll_height());
            user_scrolled_up.set(false);
            show_scroll_btn.set(false);
        }
    };

    let has_messages = move || messages_len.get() > 0;

    view! {
        <div class="chat-view">
            <Show
                when=has_agent
                fallback=move || {
                    view! {
                        <div class="chat-welcome">
                            <div class="chat-welcome-inner">
                                <img class="chat-welcome-icon" src="icon.png" alt="Tyde" />
                                <h2 class="chat-welcome-title">"Tyde"</h2>
                                <p class="chat-welcome-subtitle">"Send a message to start a conversation"</p>
                                <div class="chat-welcome-shortcuts">
                                    <span class="chat-welcome-shortcut"><kbd>"Enter"</kbd>" Send Message"</span>
                                    <span class="chat-welcome-shortcut"><kbd>"Ctrl+K"</kbd>" Command Palette"</span>
                                </div>
                            </div>
                        </div>
                    }
                }
            >
                <div class="chat-agent-header">
                    <span class="chat-agent-name">{agent_name}</span>
                    {move || agent_backend().map(|kind| {
                        let (badge_class, label) = match kind {
                            BackendKind::Tycode => ("backend-badge tycode", "Tycode"),
                            BackendKind::Kiro => ("backend-badge kiro", "Kiro"),
                            BackendKind::Claude => ("backend-badge claude", "Claude"),
                            BackendKind::Codex => ("backend-badge codex", "Codex"),
                            BackendKind::Gemini => ("backend-badge gemini", "Gemini"),
                        };
                        view! { <span class=badge_class>{label}</span> }
                    })}
                </div>
                {move || {
                    view! {
                        <TaskListView
                            task_list=task_list()
                            context_breakdown=context_breakdown.get()
                        />
                    }
                }}
                <Show when=agent_initializing>
                    <div class="chat-initializing-overlay">
                        <div class="chat-initializing-spinner"></div>
                        <p class="chat-initializing-text">"Initializing agent\u{2026}"</p>
                    </div>
                </Show>
                <div class="chat-messages-wrapper">
                    <div class="chat-messages" node_ref=scroll_ref>
                        {move || {
                            if !has_messages() && streaming().is_none() && !agent_initializing() {
                                Some(view! {
                                    <div class="chat-empty-hint">
                                        <p>"Type a message to start the conversation"</p>
                                    </div>
                                })
                            } else {
                                None
                            }
                        }}

                        <For
                            each=move || row_keys()
                            key=|k| k.clone()
                            let:k
                        >
                            <ChatMessageView agent_id=k.0 idx=k.1 />
                        </For>

                        // Transient events (retry, cancel) rendered as cards
                        {move || {
                            transient_events().map(|events| {
                                events.into_iter().map(|ev| {
                                    match ev {
                                        TransientEvent::OperationCancelled { message } => {
                                            view! {
                                                <div class="chat-card chat-card-system chat-card-cancelled">
                                                    <div class="chat-card-header">
                                                        <span class="chat-card-sender">"Cancelled"</span>
                                                    </div>
                                                    <div class="chat-card-body">
                                                        <p class="md-paragraph">{message}</p>
                                                    </div>
                                                </div>
                                            }.into_any()
                                        }
                                        TransientEvent::RetryAttempt { attempt, max_retries, error, backoff_ms } => {
                                            view! {
                                                <div class="chat-card chat-card-retry">
                                                    <div class="retry-card-header">
                                                        <span class="retry-card-icon">"⏳"</span>
                                                        <span class="retry-card-title">"Rate Limited"</span>
                                                        <span class="retry-card-attempt">{format!("Attempt {attempt} of {max_retries}")}</span>
                                                    </div>
                                                    <div class="retry-card-body">
                                                        <p class="retry-card-error">{error}</p>
                                                        <p class="retry-card-countdown">{format!("Retrying in {backoff_ms}ms\u{2026}")}</p>
                                                    </div>
                                                </div>
                                            }.into_any()
                                        }
                                    }
                                }).collect::<Vec<_>>()
                            })
                        }}

                        {move || {
                            streaming().map(|ss| view! { <ChatStreamingView streaming=ss /> })
                        }}
                    </div>

                    // Scroll-to-bottom button
                    <Show when=move || show_scroll_btn.get()>
                        <button
                            class="scroll-to-bottom-btn"
                            on:click=scroll_to_bottom
                            title="Scroll to bottom"
                        >
                            <svg width="16" height="16" viewBox="0 0 16 16" fill="none">
                                <path d="M8 3L8 13M8 13L3 8M8 13L13 8" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>
                            </svg>
                        </button>
                    </Show>
                </div>
            </Show>
            <ChatInput />
        </div>
    }
}

/// Render-layer tests for `ChatView`'s keyed message list.
///
/// Asserts on what the user perceives — DOM identity across an append. The
/// keyed `<For>` over `(agent_id, idx)` should preserve existing rows when a
/// new message is appended (only the new tail row mounts), and the in-place
/// reactive lookup inside `ChatMessageView` should project tool-request
/// mutations onto an existing row without re-mounting it.
///
/// Run with: `tools/run-wasm-tests.sh wasm_tests::` (the script handles
/// chromedriver and `wasm-bindgen-cli` setup automatically — see CLAUDE.md).
#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{ActiveAgentRef, AppState, ChatMessageEntry};
    use leptos::mount::mount_to;
    use protocol::{AgentId, ChatMessage, MessageSender};
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::{Element, HtmlElement};

    wasm_bindgen_test_configure!(run_in_browser);

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        container
            .set_attribute(
                "style",
                "position: absolute; top: 0; left: 0; width: 800px; height: 600px; \
                 display: flex; flex-direction: column;",
            )
            .unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

    fn message_rows(container: &HtmlElement) -> Vec<Element> {
        // Each `<ChatMessageView>` renders a top-level `<div class="chat-card ...">`
        // — match by the stable `chat-card` class to find the rendered rows
        // independently of the per-sender modifier classes.
        let nodes = container
            .query_selector_all(".chat-messages > .chat-card")
            .unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.item(i)?.dyn_into::<Element>().ok())
            .collect()
    }

    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    fn mk_user_msg(text: &str) -> ChatMessageEntry {
        ChatMessageEntry {
            message: ChatMessage {
                timestamp: 0,
                sender: MessageSender::User,
                content: text.to_owned(),
                reasoning: None,
                tool_calls: Vec::new(),
                model_info: None,
                token_usage: None,
                context_breakdown: None,
                images: None,
            },
            tool_requests: Vec::new(),
        }
    }

    #[wasm_bindgen_test]
    async fn appending_a_message_preserves_existing_row_identity() {
        let agent_id = AgentId("agent-1".to_owned());
        let host_id = "host-a".to_owned();

        // Bind a separate handle to the state so we can mutate it after mount.
        let state_handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let setup_handle = state_handle.clone();

        let container = make_container();
        let agent_id_for_mount = agent_id.clone();
        let host_id_for_mount = host_id.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let bound = ActiveAgentRef {
                host_id: host_id_for_mount.clone(),
                agent_id: agent_id_for_mount.clone(),
            };
            // ChatView reads its own `agent_ref` Signal prop directly; we
            // don't need to populate the global `active_agent` Memo for the
            // test to exercise the keyed-list behaviour.
            state.chat_messages.update(|m| {
                m.insert(
                    agent_id_for_mount.clone(),
                    vec![
                        mk_user_msg("first"),
                        mk_user_msg("second"),
                        mk_user_msg("third"),
                    ],
                );
            });
            *setup_handle.borrow_mut() = Some(state.clone());
            provide_context(state);
            let agent_ref_signal = Signal::derive(move || Some(bound.clone()));
            view! { <ChatView agent_ref=agent_ref_signal /> }
        });

        next_tick().await;

        let rows_before = message_rows(&container);
        assert_eq!(
            rows_before.len(),
            3,
            "expected 3 rendered rows pre-append, got {}",
            rows_before.len()
        );
        let row0_before: Element = rows_before[0].clone();
        let row2_before: Element = rows_before[2].clone();

        // Append a 4th message — the keyed `<For>` should add a single row at
        // the tail and leave rows 0..3 in place.
        let state = state_handle
            .borrow()
            .as_ref()
            .cloned()
            .expect("state captured");
        state.chat_messages.update(|m| {
            m.entry(agent_id.clone())
                .or_default()
                .push(mk_user_msg("fourth"));
        });

        next_tick().await;

        let rows_after = message_rows(&container);
        assert_eq!(
            rows_after.len(),
            4,
            "expected 4 rendered rows post-append, got {}",
            rows_after.len()
        );

        // Row identity for the existing rows must survive — proves the keyed
        // `<For>` actually keyed (and didn't rebuild the list).
        assert!(
            row0_before.is_same_node(Some(&rows_after[0])),
            "row 0 was remounted on append — keyed <For> failed"
        );
        assert!(
            row2_before.is_same_node(Some(&rows_after[2])),
            "row 2 was remounted on append — keyed <For> failed"
        );
        // Row 3 is the freshly mounted tail.
        assert_eq!(
            rows_after[3]
                .text_content()
                .unwrap_or_default()
                .contains("fourth"),
            true,
            "newly appended row should display the appended content"
        );
    }
}
