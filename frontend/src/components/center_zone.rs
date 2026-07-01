use std::cell::RefCell;

use leptos::prelude::*;
use wasm_bindgen::{JsCast, closure::Closure};
use wasm_bindgen_futures::spawn_local;

use crate::actions::begin_new_chat;
use crate::components::agent_monitor_view::AgentMonitorView;
use crate::components::chat_view::ChatView;
use crate::components::diff_view::ReviewableDiffView;
use crate::components::file_view::FileView;
use crate::components::home_view::HomeView;
use crate::components::review_view::ReviewCommentsSurface;
use crate::components::settings_panel::SettingsPanel;
use crate::components::workflow_view::WorkflowView;
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
        BackendKind::Antigravity => "Antigravity",
        BackendKind::Hermes => "Hermes",
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
            ..
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

    // Seed edit_value when editing starts (false→true transition only) and
    // focus the input once it's mounted. The two effects are deliberately
    // separate: the seeding effect must NOT subscribe to input_ref, because
    // that signal gets re-set on every element mount and would otherwise
    // clobber the user's typed value back to the original label.
    {
        let state_init = expect_context::<AppState>();
        let mut last_editing = false;
        Effect::new(move |_| {
            let editing_now = editing_tab_id.get() == Some(tab_id);
            if editing_now && !last_editing {
                let label = state_init.center_zone.with_untracked(|cz| {
                    cz.tabs
                        .iter()
                        .find(|t| t.id == tab_id)
                        .map(|t| t.label.clone())
                        .unwrap_or_default()
                });
                edit_value.set(label);
            }
            last_editing = editing_now;
        });
    }
    Effect::new(move |_| {
        if editing_tab_id.get() == Some(tab_id)
            && let Some(el) = input_ref.get()
        {
            let _ = el.focus();
            el.select();
        }
    });

    view! {
        <button
            class=move || {
                let mut class = if is_active() { "tab active" } else { "tab" }.to_string();
                if is_home_tab() {
                    class.push_str(" tab-home");
                }
                class
            }
            title=move || tab_data().map(|t| t.label).unwrap_or_default()
            aria-label=move || tab_data().map(|t| t.label).unwrap_or_default()
            data-tab-id=tab_id.0.to_string()
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
                } else if is_home_tab() {
                    view! {
                        <span class="tab-home-icon" aria-hidden="true">
                            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                <path d="M3 9l9-7 9 7v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>
                                <polyline points="9 22 9 12 15 12 15 22"/>
                            </svg>
                        </span>
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

/// Tab content variant discriminant. We track this as a `Memo` so the
/// inner-view closure inside `TabMount` only re-runs (and tears down /
/// remounts the underlying component) when the variant actually flips —
/// not on every `center_zone` update or in-place payload change. This
/// matters for tabs-disabled mode, where `replace_active` mutates the
/// active tab's content under the same `TabId`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabKind {
    Home,
    AgentMonitor,
    Chat,
    File,
    Diff,
    Comments,
    Workflow,
    Missing,
}

/// Mount a single tab's content and toggle CSS visibility based on whether
/// the tab is currently active. This preserves component-local state
/// (scroll position, find state, syntax highlight cache) across tab
/// switches — the previous implementation rebuilt the active tab's view
/// tree on every `center_zone` update, which is what made tab switching
/// feel sluggish.
///
/// The variant tracker handles tabs-disabled mode where `replace_active`
/// can mutate the active tab from one variant to another (Home → File,
/// Chat → Diff, etc.) without changing `TabId`. When the variant flips,
/// the inner closure re-runs and the previous component is unmounted.
#[component]
fn TabMount(tab_id: TabId) -> impl IntoView {
    let state = expect_context::<AppState>();

    let is_active = move || {
        state
            .center_zone
            .with(|cz| cz.active_tab_id == Some(tab_id))
    };

    let tab_kind: Memo<TabKind> = Memo::new(move |_| {
        state.center_zone.with(|cz| {
            match cz.tabs.iter().find(|t| t.id == tab_id).map(|t| &t.content) {
                Some(TabContent::Home) => TabKind::Home,
                Some(TabContent::AgentMonitor) => TabKind::AgentMonitor,
                Some(TabContent::Chat { .. }) => TabKind::Chat,
                Some(TabContent::File { .. }) => TabKind::File,
                Some(TabContent::Diff { .. }) => TabKind::Diff,
                Some(TabContent::Comments { .. }) => TabKind::Comments,
                Some(TabContent::Workflow { .. }) => TabKind::Workflow,
                None => TabKind::Missing,
            }
        })
    });

    view! {
        <div
            class="tab-mount"
            style=move || if is_active() { "" } else { "display: none;" }
        >
            {move || {
                match tab_kind.get() {
                    TabKind::Home => view! {
                        <div class="center-content-scroll">
                            <HomeView />
                        </div>
                    }.into_any(),
                    TabKind::AgentMonitor => view! {
                        <AgentMonitorView />
                    }.into_any(),
                    TabKind::Chat => {
                        // Per-tab agent_ref Signal — re-derives on the
                        // in-place `agent_ref` payload upgrade for "New
                        // Chat" tabs without remounting the ChatView.
                        let agent_ref_signal: Signal<Option<crate::state::ActiveAgentRef>> =
                            Signal::derive(move || {
                                state.center_zone.with(|cz| {
                                    match cz
                                        .tabs
                                        .iter()
                                        .find(|t| t.id == tab_id)
                                        .map(|t| &t.content)
                                    {
                                        Some(TabContent::Chat { agent_ref, .. }) => agent_ref.clone(),
                                        _ => None,
                                    }
                                })
                            });
                        // `is_active` Signal lets `ChatView` gate the
                        // `ChatInput` on active-tab. Without this every
                        // hidden chat tab kept its `ChatInput` mounted,
                        // and they all subscribe to one global
                        // `state.chat_input` — every keystroke woke each
                        // hidden input plus its textarea-autosize layout
                        // pass, scaling typing latency with tab count.
                        let active_state = state.clone();
                        let is_active_signal: Signal<bool> = Signal::derive(move || {
                            active_state
                                .center_zone
                                .with(|cz| cz.active_tab_id == Some(tab_id))
                        });
                        view! {
                            <ChatView
                                tab_id=tab_id
                                agent_ref=agent_ref_signal
                                is_active=is_active_signal
                            />
                        }.into_any()
                    }
                    TabKind::File => {
                        // Snapshot the path at the moment the variant
                        // becomes File. File tab content is immutable for
                        // a given TabId, so this snapshot stays valid for
                        // the lifetime of the variant.
                        let path = state.center_zone.with_untracked(|cz| {
                            cz.tabs
                                .iter()
                                .find(|t| t.id == tab_id)
                                .and_then(|t| match &t.content {
                                    TabContent::File { path } => Some(path.clone()),
                                    _ => None,
                                })
                        });
                        match path {
                            Some(path) => view! { <FileView tab_id=tab_id path=path /> }.into_any(),
                            None => view! { <div></div> }.into_any(),
                        }
                    }
                    TabKind::Diff => {
                        let resolved = state.center_zone.with_untracked(|cz| {
                            cz.tabs
                                .iter()
                                .find(|t| t.id == tab_id)
                                .and_then(|t| match &t.content {
                                    TabContent::Diff {
                                        host_id,
                                        project_id,
                                        root,
                                        scope,
                                        path,
                                    } => Some((
                                        host_id.clone(),
                                        project_id.clone(),
                                        root.clone(),
                                        *scope,
                                        path.clone(),
                                    )),
                                    _ => None,
                                })
                        });
                        match resolved {
                            Some((host_id, project_id, root, scope, path)) => {
                                view! { <ReviewableDiffView tab_id=tab_id host_id=host_id project_id=project_id root=root scope=scope path=path /> }.into_any()
                            }
                            None => view! { <div></div> }.into_any(),
                        }
                    }
                    TabKind::Comments => {
                        let resolved = state.center_zone.with_untracked(|cz| {
                            cz.tabs
                                .iter()
                                .find(|t| t.id == tab_id)
                                .and_then(|t| match &t.content {
                                    TabContent::Comments {
                                        host_id,
                                        project_id,
                                    } => Some((host_id.clone(), project_id.clone())),
                                    _ => None,
                                })
                        });
                        match resolved {
                            Some((host_id, project_id)) => {
                                view! { <ReviewCommentsSurface host_id=host_id project_id=project_id /> }.into_any()
                            }
                            None => view! { <div></div> }.into_any(),
                        }
                    }
                    TabKind::Workflow => {
                        // Workflow tab content is immutable for a given
                        // TabId, mirroring File/Diff.
                        let resolved = state.center_zone.with_untracked(|cz| {
                            cz.tabs
                                .iter()
                                .find(|t| t.id == tab_id)
                                .and_then(|t| match &t.content {
                                    TabContent::Workflow {
                                        agent_ref,
                                        tool_call_id,
                                    } => Some((agent_ref.clone(), tool_call_id.clone())),
                                    _ => None,
                                })
                        });
                        match resolved {
                            Some((agent_ref, tool_call_id)) => {
                                view! { <WorkflowView agent_ref=agent_ref tool_call_id=tool_call_id /> }.into_any()
                            }
                            None => view! { <div></div> }.into_any(),
                        }
                    }
                    TabKind::Missing => view! { <div></div> }.into_any(),
                }
            }}
        </div>
    }
}

#[component]
pub fn CenterZone() -> impl IntoView {
    let state = expect_context::<AppState>();

    let context_menu: RwSignal<Option<(TabId, f64, f64)>> = RwSignal::new(None);
    let editing_tab_id: RwSignal<Option<TabId>> = RwSignal::new(None);
    let tab_scroll_ref = NodeRef::<leptos::html::Div>::new();

    // New chat split button
    let menu_open = RwSignal::new(false);
    let menu_position = RwSignal::new(None::<(f64, f64)>);

    let is_connected_state = state.clone();
    let is_connected = Memo::new(move |_| {
        matches!(
            is_connected_state.chat_context_connection_status(),
            ConnectionStatus::Connected
        )
    });

    let enabled_backends_state = state.clone();
    let enabled_backends = Memo::new(move |_| {
        enabled_backends_state
            .chat_context_host_settings()
            .map(|s| s.enabled_backends)
            .unwrap_or_default()
    });

    let state_for_new_chat = state.clone();
    let on_new_chat = move |_| {
        begin_new_chat(&state_for_new_chat, None);
    };

    let state_for_agent_monitor = state.clone();
    let on_agent_monitor = move |_| {
        state_for_agent_monitor.open_tab(
            TabContent::AgentMonitor,
            "Agent Monitor".to_owned(),
            true,
        );
    };

    let on_toggle_menu = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        if menu_open.get_untracked() {
            menu_open.set(false);
            menu_position.set(None);
            return;
        }

        let Some(trigger) = ev
            .current_target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
        else {
            log::error!("new chat menu trigger click did not have an element target");
            return;
        };
        let Some(window) = web_sys::window() else {
            log::error!("new chat menu cannot open without a browser window");
            return;
        };
        let Some(window_width) = window.inner_width().ok().and_then(|width| width.as_f64()) else {
            log::error!("new chat menu cannot resolve window width");
            return;
        };
        let rect = trigger.get_bounding_client_rect();
        let top = rect.bottom() + 2.0;
        let right = (window_width - rect.right()).max(0.0);
        menu_position.set(Some((top, right)));
        menu_open.set(true);
    };

    let close_menu = move |_: web_sys::MouseEvent| {
        menu_open.set(false);
        menu_position.set(None);
    };

    let tab_ids = move || {
        state
            .center_zone
            .with(|cz| cz.tabs.iter().map(|t| t.id).collect::<Vec<_>>())
    };
    let scroll_tab_ids = move || {
        state.center_zone.with(|cz| {
            cz.tabs
                .iter()
                .filter(|t| !matches!(t.content, TabContent::Home))
                .map(|t| t.id)
                .collect::<Vec<_>>()
        })
    };
    let home_tab_id = move || {
        state.center_zone.with(|cz| {
            cz.tabs
                .iter()
                .find(|t| matches!(t.content, TabContent::Home))
                .map(|t| t.id)
        })
    };

    Effect::new(move |_| {
        let Some(active_tab_id) = state.center_zone.with(|cz| cz.active_tab_id) else {
            return;
        };
        let Some(scroller) = tab_scroll_ref.get() else {
            return;
        };

        leptos::prelude::request_animation_frame(move || {
            let selector = format!("[data-tab-id=\"{}\"]", active_tab_id.0);
            let Ok(Some(tab_el)) = scroller.query_selector(&selector) else {
                return;
            };

            let scroller_rect = scroller.get_bounding_client_rect();
            let tab_rect = tab_el.get_bounding_client_rect();
            let left_delta = tab_rect.left() - scroller_rect.left();
            let right_delta = tab_rect.right() - scroller_rect.right();
            let padding = 8.0;
            let current_scroll = scroller.scroll_left();

            if left_delta < padding {
                scroller.set_scroll_left(current_scroll + (left_delta - padding).round() as i32);
            } else if right_delta > -padding {
                scroller.set_scroll_left(current_scroll + (right_delta + padding).round() as i32);
            }
        });
    });

    // Tabs whose content components should currently exist in the DOM:
    // the active tab plus up to `TAB_LRU_CAPACITY - 1` recently-active
    // tabs. Other tabs still appear in the strip but their content is
    // unmounted — switching to one remounts it from cached AppState
    // (`chat_rows`, `open_files`, `diff_contents`) so no data is lost,
    // only ephemeral UI state like scroll offset.
    //
    // We include the current `active_tab_id` unconditionally even when
    // it's not yet in `tab_lru`. The LRU-bumping Effect in `App` runs
    // *after* the synchronous `center_zone.update` that switched the
    // active tab — without this safety net there's a one-frame window
    // where the new active tab isn't in the LRU and so doesn't render,
    // visible to the user as a flash of empty center zone on every tab
    // switch.
    let mounted_tab_ids = move || {
        let lru = state.tab_lru.get();
        // Filter to keep `cz.tabs` order — a stable order avoids surprising
        // `<For>` keyed-diff thrash when the LRU MRU-front churns.
        state.center_zone.with(|cz| {
            let active = cz.active_tab_id;
            cz.tabs
                .iter()
                .filter(|t| Some(t.id) == active || lru.contains(&t.id))
                .map(|t| t.id)
                .collect::<Vec<_>>()
        })
    };

    let tab_bar_class = move || {
        if state.tabs_enabled.get() {
            "tab-bar center-tab-bar"
        } else {
            "tab-bar center-tab-bar tab-bar-hidden"
        }
    };

    view! {
        <div class="center-zone">
            <div class=tab_bar_class>
                <div class="pinned-tab-leading">
                    {move || home_tab_id().map(|id| {
                        view! { <TabButton tab_id=id context_menu=context_menu editing_tab_id=editing_tab_id /> }
                    })}
                </div>

                <div class="tab-strip-scroll" node_ref=tab_scroll_ref>
                    <For
                        each=move || scroll_tab_ids()
                        key=|id| *id
                        let:id
                    >
                        <TabButton tab_id=id context_menu=context_menu editing_tab_id=editing_tab_id />
                    </For>
                </div>

                <div class="pinned-tab-actions">
                    <span class="tab-bar-divider" aria-hidden="true"></span>
                    <button
                        class="center-tool-btn"
                        title="Open Agent Monitor"
                        aria-label="Open Agent Monitor"
                        on:click=on_agent_monitor
                    >
                        "Agents"
                    </button>
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

                        <Show when=move || menu_open.get() && menu_position.get().is_some()>
                            <div class="new-chat-backdrop" on:click=close_menu></div>
                            <div
                                class="new-chat-menu"
                                role="menu"
                                style=move || {
                                    menu_position
                                        .get()
                                        .map(|(top, right)| {
                                            format!("top: {top}px; right: {right}px;")
                                        })
                                        .unwrap_or_default()
                                }
                            >
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
                                                menu_position.set(None);
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
            </div>
            <div class="center-content">
                <For
                    each=move || mounted_tab_ids()
                    key=|id| *id
                    let:tab_id
                >
                    <TabMount tab_id=tab_id />
                </For>
                {move || {
                    if tab_ids().is_empty() {
                        Some(view! {
                            <div class="center-content-scroll">
                                <HomeView />
                            </div>
                        })
                    } else {
                        None
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

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use leptos::mount::mount_to;
    use wasm_bindgen_test::*;
    use web_sys::{HtmlElement, HtmlInputElement};

    wasm_bindgen_test_configure!(run_in_browser);

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
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

    fn click_context_menu_item(container: &HtmlElement, text: &str) {
        let buttons = container
            .query_selector_all(".context-menu button")
            .unwrap();
        for i in 0..buttons.length() {
            let button = buttons.item(i).unwrap().dyn_into::<HtmlElement>().unwrap();
            if button.text_content().as_deref().map(str::trim) == Some(text) {
                button.click();
                return;
            }
        }
        panic!("context menu item {text:?} not found");
    }

    #[wasm_bindgen_test]
    async fn chat_tab_rename_survives_external_label_update() {
        let container = make_container();
        let state = AppState::new();
        state.open_tab(TabContent::empty_chat(), "Original Chat".to_owned(), true);
        let chat_tab_id = state
            .center_zone
            .with_untracked(|cz| cz.active_tab_id.expect("chat tab active"));

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <CenterZone /> }
        });
        next_tick().await;

        let tab_button: HtmlElement = container
            .query_selector(".tab-strip-scroll button.tab")
            .unwrap()
            .expect("chat tab button")
            .dyn_into()
            .unwrap();
        let contextmenu = web_sys::MouseEvent::new("contextmenu").unwrap();
        tab_button.dispatch_event(&contextmenu).unwrap();
        next_tick().await;
        click_context_menu_item(&container, "Rename");
        next_tick().await;

        let document = web_sys::window().unwrap().document().unwrap();
        let input: HtmlInputElement = container
            .query_selector("input.tab-rename-input")
            .unwrap()
            .expect("rename input should be visible")
            .dyn_into()
            .unwrap();
        assert_eq!(
            input.value(),
            "Original Chat",
            "rename input should seed from the tab label when editing starts"
        );
        let input_node: web_sys::Element = input.clone().dyn_into().unwrap();
        let active = document.active_element().expect("focused element");
        assert!(
            active.is_same_node(Some(&input_node)),
            "rename input should be focused when editing starts"
        );

        input.set_value("User Typed Title");
        input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;

        state.rename_tab_label(chat_tab_id, "External Session Label".to_owned());
        next_tick().await;

        let current_input: HtmlInputElement = container
            .query_selector("input.tab-rename-input")
            .unwrap()
            .expect("external label update must not exit rename mode")
            .dyn_into()
            .unwrap();
        let current_node: web_sys::Element = current_input.clone().dyn_into().unwrap();
        assert!(
            input_node.is_same_node(Some(&current_node)),
            "external label update remounted the rename input"
        );
        assert_eq!(
            current_input.value(),
            "User Typed Title",
            "external label update must not clobber the in-progress rename"
        );
        let active = document.active_element().expect("focused element");
        assert!(
            active.is_same_node(Some(&input_node)),
            "external label update blurred the rename input"
        );
    }
}
