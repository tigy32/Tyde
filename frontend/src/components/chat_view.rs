use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use crate::components::chat_input::ChatInput;
use crate::components::chat_message::ChatMessageView;
use crate::components::chat_streaming::ChatStreamingView;
use crate::components::settings_panel::persist_tool_output_mode;
use crate::components::task_list::TaskListView;
use crate::state::{ActiveAgentRef, AgentInfo, AppState, ChatRowHandle, ToolOutputMode, TransientEvent};

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
    /// True only when this tab is the active one in the center-zone.
    /// Used to gate the `ChatInput` so hidden chat tabs don't mount
    /// duplicate inputs that all subscribe to the global
    /// `state.chat_input` — every keystroke would wake each hidden
    /// instance, doubling-or-worse the per-keystroke main-thread cost.
    is_active: Signal<bool>,
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
            .chat_rows
            .with(|m| m.get(&id).map(|v| v.len()).unwrap_or(0)),
        None => 0,
    });

    let row_handles = move || -> Vec<ChatRowHandle> {
        let Some(id) = active_agent_id() else {
            return Vec::new();
        };
        state
            .chat_rows
            .with(|m| m.get(&id).cloned().unwrap_or_default())
    };

    // `.with` reads through the HashMap signals without cloning the
    // entire map — the previous `.get()` allocated a fresh
    // HashMap<AgentId, StreamingState> on every read, and these
    // closures fire from the auto-scroll Effect on every stream-start
    // / stream-end, plus per-render in the streaming-card branch.
    let streaming = move || {
        let agent_id = agent_ref.get()?.agent_id;
        state.streaming_text.with(|m| m.get(&agent_id).cloned())
    };

    let task_list = move || {
        let agent_id = agent_ref.get()?.agent_id;
        state.task_lists.with(|m| m.get(&agent_id).cloned())
    };

    // Walk back from the latest message to find the most recent assistant
    // message that carries a context_breakdown. `ContextBreakdown` does not
    // implement `PartialEq`, so we use a derived Signal rather than a Memo.
    // Each read still walks the vec, but it's bounded by "messages up to the
    // most recent assistant turn" — typically a single iteration.
    let context_breakdown: Signal<Option<protocol::ContextBreakdown>> = Signal::derive(move || {
        let id = active_agent_id()?;
        state.chat_rows.with(|m| {
            let rows = m.get(&id)?;
            for row in rows.iter().rev() {
                let entry = row.entry.get();
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
        state.transient_events.with(|m| m.get(&agent_id).cloned())
    };

    // Centralised lookup of the AgentInfo for this view's agent_ref.
    // The previous code did `state.agents.get()` (clones the full Vec)
    // three times across `agent_name`, `agent_backend`, and
    // `agent_initializing`, so any agent-list change fired three full
    // clones. Sharing a single `Memo<Option<AgentInfo>>` collapses
    // that to one clone per change, with closures becoming cheap
    // field reads.
    let current_agent: Memo<Option<AgentInfo>> = Memo::new(move |_| {
        let active = agent_ref.get()?;
        state.agents.with(|agents| {
            agents
                .iter()
                .find(|a| a.host_id == active.host_id && a.agent_id == active.agent_id)
                .cloned()
        })
    });

    let agent_name = move || -> String {
        if agent_ref.get().is_none() {
            return String::new();
        }
        current_agent
            .get()
            .map(|a| a.name)
            .unwrap_or_else(|| "[unknown agent]".to_owned())
    };

    let agent_backend = move || -> Option<BackendKind> { current_agent.get().map(|a| a.backend_kind) };

    let agent_initializing = move || -> bool {
        current_agent
            .get()
            .map(|a| !a.started && a.fatal_error.is_none())
            .unwrap_or(false)
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
    //
    // The scroll handler reads `scrollHeight`/`scrollTop`/`clientHeight`,
    // each of which forces a synchronous layout. Trackpad/wheel scrolls
    // can fire 100+ events per second, so we throttle to one read per
    // animation frame. We also only `set` on the signals when the
    // boolean actually changes — a stationary signal write still
    // notifies subscribers in Leptos's RwSignal.
    let scroll_ref_for_handler = scroll_ref;
    Effect::new(move |_| {
        if let Some(el) = scroll_ref_for_handler.get() {
            let el_clone = el.clone();
            let listener_pending = std::rc::Rc::new(std::cell::Cell::new(false));
            let last_is_near_bottom = std::rc::Rc::new(std::cell::Cell::new(true));
            let handler = Closure::<dyn Fn()>::new(move || {
                if listener_pending.get() {
                    return;
                }
                listener_pending.set(true);
                let pending = listener_pending.clone();
                let last = last_is_near_bottom.clone();
                let el_for_raf = el_clone.clone();
                leptos::prelude::request_animation_frame(move || {
                    pending.set(false);
                    let scroll_height = el_for_raf.scroll_height();
                    let scroll_top = el_for_raf.scroll_top();
                    let client_height = el_for_raf.client_height();
                    let distance_from_bottom = scroll_height - scroll_top - client_height;
                    let is_near_bottom = distance_from_bottom < 80;
                    if is_near_bottom != last.get() {
                        last.set(is_near_bottom);
                        user_scrolled_up.set(!is_near_bottom);
                        show_scroll_btn.set(!is_near_bottom);
                    }
                });
            });
            let _ = el.add_event_listener_with_callback("scroll", handler.as_ref().unchecked_ref());
            handler.forget();
        }
    });

    // Auto-scroll effect: whenever the message count or streaming text grows,
    // scroll to bottom (only if the user hasn't scrolled up). Scoped to the
    // *length* of messages — not the full Vec — so unrelated chat row
    // updates (e.g. tool_request mutations to existing rows) don't trigger a
    // scroll.
    //
    // Coalesce multiple deltas-per-frame into a single rAF. The previous
    // implementation scheduled one rAF per `text`/`reasoning` delta — at
    // 50+ deltas/sec while the model streams, all of them fired in the
    // *same* frame and each ran its own scrollHeight read (a forced
    // layout) plus a scrollTop write. The pending-flag gate caps it to
    // at most one scroll per frame, which still keeps the bottom
    // pinned.
    let scroll_pending = std::rc::Rc::new(std::cell::Cell::new(false));
    Effect::new(move |_| {
        let _len = messages_len.get();
        let stream = streaming();
        if let Some(ss) = stream.as_ref() {
            // Subscribe without cloning the strings. `.get()` on
            // `ArcRwSignal<String>` cloned the entire accumulated text
            // into a temporary just to be discarded — `.with` reads
            // through and tracks the dependency without the alloc.
            ss.text.with(|_| ());
            ss.reasoning.with(|_| ());
        }
        if user_scrolled_up.get_untracked() {
            return;
        }
        if scroll_pending.get() {
            return;
        }
        scroll_pending.set(true);
        let pending = scroll_pending.clone();
        request_animation_frame(move || {
            pending.set(false);
            // rAF callback runs outside the reactive tracking context;
            // `.get_untracked()` reads the NodeRef without registering
            // a (useless) subscription that Leptos would warn about.
            if let Some(el) = scroll_ref.get_untracked() {
                el.set_scroll_top(el.scroll_height());
            }
        });
    });

    let scroll_to_bottom = move |_| {
        // Event handler — not a reactive context, so use untracked
        // read on the NodeRef.
        if let Some(el) = scroll_ref.get_untracked() {
            el.set_scroll_top(el.scroll_height());
            user_scrolled_up.set(false);
            show_scroll_btn.set(false);
        }
    };

    let has_messages = move || messages_len.get() > 0;

    // (ToolOutputModeToggle is defined below.)

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
                    <ToolOutputModeToggle />
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
                            each=move || row_handles()
                            key=|row| row.id
                            let:row
                        >
                            <ChatMessageView row=row />
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
            <Show
                when=move || is_active.get()
                fallback=|| ()
            >
                <ChatInput />
            </Show>
        </div>
    }
}

/// Cycle button for the global tool-output verbosity setting. Lives on the
/// chat header next to the backend badge. Reads and writes
/// `state.tool_output_mode` directly (frontend-local, persisted to
/// localStorage); never goes through the protocol.
#[component]
fn ToolOutputModeToggle() -> impl IntoView {
    let state = expect_context::<AppState>();
    let mode = state.tool_output_mode;

    let label = move || match mode.get() {
        ToolOutputMode::Summary => "\u{2298}",
        ToolOutputMode::Compact => "\u{25d0}",
        ToolOutputMode::Full => "\u{25c9}",
    };
    let title = move || match mode.get() {
        ToolOutputMode::Summary => "Tool output: summary (click to switch to compact)",
        ToolOutputMode::Compact => "Tool output: compact (click to switch to full)",
        ToolOutputMode::Full => "Tool output: full (click to switch to summary)",
    };

    let on_click = move |_| {
        let next = match mode.get_untracked() {
            ToolOutputMode::Summary => ToolOutputMode::Compact,
            ToolOutputMode::Compact => ToolOutputMode::Full,
            ToolOutputMode::Full => ToolOutputMode::Summary,
        };
        mode.set(next);
        persist_tool_output_mode(next);
    };

    view! {
        <button
            class="tool-output-mode-toggle"
            title=title
            on:click=on_click
        >{label}</button>
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
            state.chat_rows.update(|m| {
                m.insert(
                    agent_id_for_mount.clone(),
                    vec![
                        ChatRowHandle::new(mk_user_msg("first")),
                        ChatRowHandle::new(mk_user_msg("second")),
                        ChatRowHandle::new(mk_user_msg("third")),
                    ],
                );
            });
            *setup_handle.borrow_mut() = Some(state.clone());
            provide_context(state);
            let agent_ref_signal = Signal::derive(move || Some(bound.clone()));
            let is_active_signal: Signal<bool> = Signal::derive(|| true);
            view! { <ChatView agent_ref=agent_ref_signal is_active=is_active_signal /> }
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
        state.push_chat_entry(agent_id.clone(), mk_user_msg("fourth"));

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
