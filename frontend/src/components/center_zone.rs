use std::cell::RefCell;

use leptos::prelude::*;
use wasm_bindgen::{JsCast, closure::Closure};
use wasm_bindgen_futures::spawn_local;

use crate::actions::begin_new_chat;
use crate::components::chat_view::ChatView;
use crate::components::diff_view::DiffView;
use crate::components::file_view::FileView;
use crate::components::home_view::HomeView;
use crate::components::settings_panel::SettingsPanel;
use crate::send::send_frame;
use crate::state::{AppState, ConnectionStatus, TabContent, TabId};

use protocol::{BackendKind, FrameKind, SetAgentNamePayload};

struct EscListenerHandle {
    window: web_sys::Window,
    callback: Closure<dyn Fn(web_sys::Event)>,
}

thread_local! {
    static ESC_LISTENER: RefCell<Option<EscListenerHandle>> = const { RefCell::new(None) };
}

fn clear_esc_listener() {
    ESC_LISTENER.with(|slot| {
        if let Some(handle) = slot.borrow_mut().take() {
            let _ = handle.window.remove_event_listener_with_callback(
                "keydown",
                handle.callback.as_ref().unchecked_ref(),
            );
        }
    });
}

fn backend_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "Tycode",
        BackendKind::Kiro => "Kiro",
        BackendKind::Claude => "Claude",
        BackendKind::Codex => "Codex",
        BackendKind::Gemini => "Gemini",
    }
}

fn do_rename(state: AppState, tab_id: TabId, new_label: String) {
    let content = state.center_zone.with_untracked(|cz| {
        cz.tabs
            .iter()
            .find(|t| t.id == tab_id)
            .map(|t| t.content.clone())
    });
    match content {
        Some(TabContent::Chat {
            agent_ref: Some(agent_ref),
        }) => {
            let agent_info = state.agents.with_untracked(|agents| {
                agents
                    .iter()
                    .find(|a| a.host_id == agent_ref.host_id && a.agent_id == agent_ref.agent_id)
                    .cloned()
            });
            match agent_info {
                Some(agent) => {
                    let host_id = agent.host_id.clone();
                    let stream = agent.instance_stream.clone();
                    spawn_local(async move {
                        if let Err(e) = send_frame(
                            &host_id,
                            stream,
                            FrameKind::SetAgentName,
                            &SetAgentNamePayload { name: new_label },
                        )
                        .await
                        {
                            log::error!("failed to send SetAgentName: {e}");
                        }
                    });
                }
                None => {
                    log::error!("cannot rename tab {tab_id:?}: agent not found");
                }
            }
        }
        Some(_) => {
            state.rename_tab_label(tab_id, new_label);
        }
        None => {}
    }
}

#[component]
fn TabContextMenu(
    tab_id: TabId,
    x: f64,
    y: f64,
    context_menu: RwSignal<Option<(TabId, f64, f64)>>,
    editing_tab_id: RwSignal<Option<TabId>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    let is_closeable = move || {
        state.center_zone.with(|cz| {
            cz.tabs
                .iter()
                .find(|t| t.id == tab_id)
                .map(|t| t.closeable)
                .unwrap_or(false)
        })
    };

    let has_closeable_to_right = move || {
        state.center_zone.with(|cz| {
            let Some(idx) = cz.tabs.iter().position(|t| t.id == tab_id) else {
                return false;
            };
            cz.tabs[idx + 1..].iter().any(|t| t.closeable)
        })
    };

    // Window keydown listener for Escape dismissal — stored in thread_local so
    // on_cleanup can use a plain fn pointer (required to be Send+Sync by Leptos).
    clear_esc_listener();
    let esc_cb = Closure::<dyn Fn(web_sys::Event)>::new(move |ev: web_sys::Event| {
        if let Ok(kev) = ev.dyn_into::<web_sys::KeyboardEvent>()
            && kev.key() == "Escape"
        {
            context_menu.set(None);
        }
    });
    let window = web_sys::window().unwrap();
    let _ = window.add_event_listener_with_callback("keydown", esc_cb.as_ref().unchecked_ref());
    ESC_LISTENER.with(|slot| {
        slot.borrow_mut().replace(EscListenerHandle {
            window,
            callback: esc_cb,
        });
    });
    on_cleanup(clear_esc_listener);

    view! {
        // Backdrop — catches click-outside to dismiss
        <div
            style="position: fixed; inset: 0; z-index: 1000;"
            on:click=move |_| context_menu.set(None)
            on:contextmenu=move |ev: web_sys::MouseEvent| {
                ev.prevent_default();
                context_menu.set(None);
            }
        />
        // Menu
        <div
            class="context-menu"
            style=format!("left: {}px; top: {}px;", x, y)
            on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()
        >
            {move || is_closeable().then(|| view! {
                <button
                    class="context-menu-item"
                    on:click=move |_| {
                        context_menu.set(None);
                        editing_tab_id.set(Some(tab_id));
                    }
                >
                    "Rename"
                </button>
                <button
                    class="context-menu-item"
                    on:click=move |_| {
                        context_menu.set(None);
                        let state = expect_context::<AppState>();
                        state.close_tab(tab_id);
                    }
                >
                    "Close"
                </button>
            })}
            <button
                class="context-menu-item"
                on:click=move |_| {
                    context_menu.set(None);
                    let state = expect_context::<AppState>();
                    state.close_other_tabs(tab_id);
                }
            >
                "Close Other Tabs"
            </button>
            {move || has_closeable_to_right().then(|| view! {
                <button
                    class="context-menu-item"
                    on:click=move |_| {
                        context_menu.set(None);
                        let state = expect_context::<AppState>();
                        state.close_tabs_to_right(tab_id);
                    }
                >
                    "Close Tabs to the Right"
                </button>
            })}
            <button
                class="context-menu-item"
                on:click=move |_| {
                    context_menu.set(None);
                    let state = expect_context::<AppState>();
                    state.close_all_tabs();
                }
            >
                "Close All Tabs"
            </button>
        </div>
    }
}

#[component]
fn TabButton(
    tab_id: TabId,
    context_menu: RwSignal<Option<(TabId, f64, f64)>>,
    editing_tab_id: RwSignal<Option<TabId>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    let tab_data = move || {
        state
            .center_zone
            .with(|cz| cz.tabs.iter().find(|t| t.id == tab_id).cloned())
    };
    let is_active = move || {
        state
            .center_zone
            .with(|cz| cz.active_tab_id == Some(tab_id))
    };
    let is_closeable = move || tab_data().is_some_and(|t| t.closeable);
    let is_home_tab = move || tab_data().is_some_and(|t| matches!(t.content, TabContent::Home));
    let is_editing = move || editing_tab_id.get() == Some(tab_id);

    let on_click = move |_| state.activate_tab(tab_id);

    let on_contextmenu = move |ev: web_sys::MouseEvent| {
        ev.prevent_default();
        if is_home_tab() {
            return;
        }
        context_menu.set(Some((tab_id, ev.client_x() as f64, ev.client_y() as f64)));
    };

    let input_ref = NodeRef::<leptos::html::Input>::new();
    let edit_value: RwSignal<String> = RwSignal::new(String::new());

    // Initialize edit_value and focus the input when editing starts
    {
        let state_init = expect_context::<AppState>();
        Effect::new(move |_| {
            if editing_tab_id.get() == Some(tab_id) {
                let label = state_init.center_zone.with_untracked(|cz| {
                    cz.tabs
                        .iter()
                        .find(|t| t.id == tab_id)
                        .map(|t| t.label.clone())
                        .unwrap_or_default()
                });
                edit_value.set(label);
                if let Some(el) = input_ref.get() {
                    let _ = el.focus();
                    el.select();
                }
            }
        });
    }

    view! {
        <button
            class=move || if is_active() { "tab active" } else { "tab" }
            on:click=on_click
            on:contextmenu=on_contextmenu
        >
            {move || {
                if is_editing() {
                    let state_kd = expect_context::<AppState>();
                    let state_bl = expect_context::<AppState>();
                    let on_keydown = move |ev: web_sys::KeyboardEvent| {
                        ev.stop_propagation();
                        match ev.key().as_str() {
                            "Enter" => {
                                let label = edit_value.get_untracked().trim().to_string();
                                editing_tab_id.set(None);
                                if !label.is_empty() {
                                    do_rename(state_kd.clone(), tab_id, label);
                                }
                            }
                            "Escape" => editing_tab_id.set(None),
                            _ => {}
                        }
                    };
                    let on_blur = move |_: web_sys::FocusEvent| {
                        if editing_tab_id.with_untracked(|e| *e != Some(tab_id)) {
                            return;
                        }
                        let label = edit_value.get_untracked().trim().to_string();
                        editing_tab_id.set(None);
                        if !label.is_empty() {
                            do_rename(state_bl.clone(), tab_id, label);
                        }
                    };
                    view! {
                        <input
                            type="text"
                            class="tab-rename-input"
                            node_ref=input_ref
                            spellcheck="false"
                            {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                            autocapitalize="none"
                            autocomplete="off"
                            prop:value=move || edit_value.get()
                            on:input=move |ev| edit_value.set(event_target_value(&ev))
                            on:keydown=on_keydown
                            on:blur=on_blur
                            on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()
                        />
                    }.into_any()
                } else {
                    view! {
                        <span class="tab-label">{move || tab_data().map(|t| t.label).unwrap_or_default()}</span>
                    }.into_any()
                }
            }}
            {move || is_closeable().then(|| {
                let on_close = move |ev: web_sys::MouseEvent| {
                    ev.stop_propagation();
                    let state = expect_context::<AppState>();
                    state.close_tab(tab_id);
                };
                view! {
                    <span class="tab-close" on:click=on_close>
                        <svg width="8" height="8" viewBox="0 0 8 8" fill="none">
                            <path d="M1 1L7 7M7 1L1 7" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>
                        </svg>
                    </span>
                }
            })}
        </button>
    }
}

#[component]
pub fn CenterZone() -> impl IntoView {
    let state = expect_context::<AppState>();

    let context_menu: RwSignal<Option<(TabId, f64, f64)>> = RwSignal::new(None);
    let editing_tab_id: RwSignal<Option<TabId>> = RwSignal::new(None);

    // New chat split button
    let menu_open = RwSignal::new(false);

    let is_connected_state = state.clone();
    let is_connected = Memo::new(move |_| {
        matches!(
            is_connected_state.selected_host_connection_status(),
            ConnectionStatus::Connected
        )
    });

    let enabled_backends_state = state.clone();
    let enabled_backends = Memo::new(move |_| {
        enabled_backends_state
            .selected_host_settings()
            .map(|s| s.enabled_backends)
            .unwrap_or_default()
    });

    let state_for_new_chat = state.clone();
    let on_new_chat = move |_| {
        begin_new_chat(&state_for_new_chat, None);
    };

    let on_toggle_menu = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        menu_open.update(|open| *open = !*open);
    };

    let close_menu = move |_: web_sys::MouseEvent| {
        menu_open.set(false);
    };

    let tab_ids = move || {
        state
            .center_zone
            .with(|cz| cz.tabs.iter().map(|t| t.id).collect::<Vec<_>>())
    };

    let tab_bar_class = move || {
        if state.tabs_enabled.get() {
            "tab-bar"
        } else {
            "tab-bar tab-bar-hidden"
        }
    };

    view! {
        <div class="center-zone">
            <div class=tab_bar_class>
                {move || tab_ids().into_iter().map(|id| {
                    view! { <TabButton tab_id=id context_menu=context_menu editing_tab_id=editing_tab_id /> }
                }).collect_view()}

                <div class="tab-bar-spacer"></div>

                <div class="new-chat-split">
                    <button
                        class="new-chat-btn"
                        title="New Chat"
                        disabled=move || !is_connected.get()
                        on:click=on_new_chat
                    >
                        "New Chat"
                    </button>
                    <button
                        class="new-chat-menu-trigger"
                        title="Choose backend for new chat"
                        disabled=move || !is_connected.get()
                        on:click=on_toggle_menu
                        aria-haspopup="menu"
                        aria-expanded=move || if menu_open.get() { "true" } else { "false" }
                    >
                        <svg width="10" height="6" viewBox="0 0 10 6" fill="none">
                            <path d="M1 1L5 5L9 1" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>
                        </svg>
                    </button>

                    <Show when=move || menu_open.get()>
                        <div class="new-chat-backdrop" on:click=close_menu></div>
                        <div class="new-chat-menu" role="menu">
                            {move || {
                                let backends = enabled_backends.get();
                                if backends.is_empty() {
                                    vec![view! {
                                        <div class="new-chat-menu-empty">
                                            "No backends enabled"
                                        </div>
                                    }.into_any()]
                                } else {
                                    backends.into_iter().map(|kind| {
                                        let label = backend_label(kind);
                                        let menu_state = expect_context::<AppState>();
                                        let on_click = move |_| {
                                            menu_open.set(false);
                                            begin_new_chat(&menu_state, Some(kind));
                                        };
                                        view! {
                                            <button
                                                class="new-chat-menu-item"
                                                role="menuitem"
                                                on:click=on_click
                                            >
                                                {format!("New {label} Chat")}
                                            </button>
                                        }.into_any()
                                    }).collect::<Vec<_>>()
                                }
                            }}
                        </div>
                    </Show>
                </div>
            </div>
            <div class="center-content">
                {move || {
                    let cz = state.center_zone.get();
                    let active_tab = cz.active_tab_id
                        .and_then(|id| cz.tabs.iter().find(|t| t.id == id));
                    match active_tab.map(|t| &t.content) {
                        Some(TabContent::Home) => view! {
                            <div class="center-content-scroll">
                                <HomeView />
                            </div>
                        }.into_any(),
                        Some(TabContent::Chat { agent_ref }) => {
                            state.active_agent.set(agent_ref.clone());
                            view! { <ChatView /> }.into_any()
                        }
                        Some(TabContent::File { path }) => {
                            view! { <FileView path=path.clone() /> }.into_any()
                        }
                        Some(TabContent::Diff { root, scope }) => {
                            view! { <DiffView root=root.clone() scope=*scope /> }.into_any()
                        }
                        None => view! {
                            <div class="center-content-scroll">
                                <HomeView />
                            </div>
                        }.into_any(),
                    }
                }}
            </div>
            <SettingsPanel />
            {move || context_menu.get().map(|(cm_tab_id, x, y)| {
                view! {
                    <TabContextMenu
                        tab_id=cm_tab_id
                        x=x
                        y=y
                        context_menu=context_menu
                        editing_tab_id=editing_tab_id
                    />
                }
            })}
        </div>
    }
}
