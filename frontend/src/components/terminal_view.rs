use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::spawn_local;

use crate::send::send_frame;
use crate::state::{AppState, DockVisibility, TerminalInfo, root_display_name};
use crate::term_bridge;

use protocol::{
    FrameKind, ProjectRootPath, StreamPath, TerminalClosePayload, TerminalCreatePayload,
    TerminalId, TerminalLaunchTarget, TerminalResizePayload, TerminalSendPayload,
};

#[component]
pub fn TerminalView() -> impl IntoView {
    let state = expect_context::<AppState>();

    let terminals = move || state.terminals.get();

    let state_for_empty = state.clone();
    let show_empty = move || state_for_empty.terminals.get().is_empty();

    view! {
        <div class="terminal-view">
            <TerminalTabBar />
            <div class="terminal-body">
                <Show when=show_empty>
                    <div class="terminal-empty">
                        <span class="terminal-empty-text">"No terminal open"</span>
                    </div>
                </Show>
                // Render every terminal; inactive ones are hidden via CSS so
                // their xterm instance stays mounted and scrollback survives
                // tab switches.
                <For
                    each=terminals
                    key=|t| (t.host_id.clone(), t.terminal_id.clone())
                    let:term
                >
                    <TerminalContent term=term />
                </For>
            </div>
        </div>
    }
}

#[component]
fn TerminalTabBar() -> impl IntoView {
    let state = expect_context::<AppState>();
    let state_for_can_create = state.clone();
    let selected_root = RwSignal::new(None::<ProjectRootPath>);

    let terminal_roots = Memo::new(move |_| {
        let active_project = state_for_can_create.active_project.get()?;
        let project = state_for_can_create
            .projects
            .get()
            .into_iter()
            .find(|project| {
                project.host_id == active_project.host_id
                    && project.project.id == active_project.project_id
            })?;
        Some(project.project.root_paths())
    });

    let state_for_new_terminal = state.clone();
    let terminal_roots_for_new = terminal_roots;
    let selected_root_for_new = selected_root;
    let on_new_terminal = move |_| {
        let (host_id, host_stream, target) = match terminal_create_request(
            &state_for_new_terminal,
            terminal_roots_for_new.get(),
            selected_root_for_new.get(),
        ) {
            Some(v) => v,
            None => return,
        };

        let payload = TerminalCreatePayload {
            target,
            cols: 80,
            rows: 24,
        };

        state_for_new_terminal
            .bottom_dock
            .set(DockVisibility::Visible);

        spawn_local(async move {
            if let Err(e) =
                send_frame(&host_id, host_stream, FrameKind::TerminalCreate, &payload).await
            {
                log::error!("failed to create terminal: {e}");
            }
        });
    };

    let state_for_disabled = state.clone();
    let terminal_roots_for_disabled = terminal_roots;
    let selected_root_for_disabled = selected_root;
    let btn_disabled = move || {
        terminal_create_request(
            &state_for_disabled,
            terminal_roots_for_disabled.get(),
            selected_root_for_disabled.get(),
        )
        .is_none()
    };
    let state_for_tabs = state.clone();
    let root_options = terminal_roots;
    let selected_root_for_value = selected_root;
    let selected_root_for_change = selected_root;

    view! {
        <div class="terminal-tab-bar">
            <div class="terminal-tabs">
                <For
                    each=move || state_for_tabs.terminals.get()
                    key=|t| t.terminal_id.clone()
                    let:term
                >
                    <TerminalTab host_id=term.host_id terminal_id=term.terminal_id />
                </For>
            </div>
            <Show when=move || root_options.get().is_some_and(|roots| roots.len() > 1)>
                <select
                    class="terminal-root-select"
                    prop:value=move || {
                        selected_root_for_value
                            .get()
                            .map(|root| root.0)
                            .or_else(|| root_options.get().and_then(|roots| roots.first().map(|root| root.0.clone())))
                            .unwrap_or_default()
                    }
                    on:change=move |ev| {
                        selected_root_for_change.set(Some(ProjectRootPath(event_target_value(&ev))));
                    }
                    title="Terminal root"
                >
                    {move || {
                        root_options
                            .get()
                            .unwrap_or_default()
                            .into_iter()
                            .map(|root| {
                                let value = root.0.clone();
                                let label = root_display_name(&root);
                                view! {
                                    <option value=value>{label}</option>
                                }
                            })
                            .collect_view()
                    }}
                </select>
            </Show>
            <button
                class="terminal-new-btn"
                on:click=on_new_terminal
                title="New Terminal"
                disabled=btn_disabled
            >
                "+"
            </button>
        </div>
    }
}

fn terminal_create_request(
    state: &AppState,
    project_roots: Option<Vec<ProjectRootPath>>,
    selected_root: Option<ProjectRootPath>,
) -> Option<(String, StreamPath, TerminalLaunchTarget)> {
    let (host_id, target) = match state.active_project.get() {
        Some(active_project) => {
            let roots = project_roots?;
            let root = selected_root
                .filter(|selected| roots.iter().any(|root| root == selected))
                .or_else(|| roots.first().cloned())?;
            let host_id = active_project.host_id;
            (
                host_id,
                TerminalLaunchTarget::Project {
                    project_id: active_project.project_id,
                    root,
                    relative_cwd: None,
                },
            )
        }
        None => (
            state.selected_host_id.get()?,
            TerminalLaunchTarget::HostDefault,
        ),
    };
    let host_stream = state.host_streams.get().get(&host_id)?.clone();
    Some((host_id, host_stream, target))
}

#[component]
fn TerminalTab(host_id: String, terminal_id: TerminalId) -> impl IntoView {
    let state = expect_context::<AppState>();

    let state_for_term = state.clone();
    let host_id_for_term = host_id.clone();
    let tid_for_term = terminal_id.clone();
    let term = move || {
        state_for_term
            .terminals
            .get()
            .into_iter()
            .find(|t| t.host_id == host_id_for_term && t.terminal_id == tid_for_term)
    };

    let term_for_label = term.clone();
    let tid_for_label = terminal_id.clone();
    let label = move || match term_for_label() {
        Some(t) if !t.shell.is_empty() => {
            if let Some(root) = &t.root {
                format!("{} · {}", root_display_name(root), t.shell)
            } else {
                t.shell
            }
        }
        _ => format!("Terminal {}", short_id(&tid_for_label)),
    };

    let term_for_exited = term.clone();
    let exited = move || term_for_exited().is_some_and(|t| t.exited);

    let state_for_active = state.clone();
    let host_id_for_active = host_id.clone();
    let tid_for_active = terminal_id.clone();
    let is_active = move || {
        state_for_active
            .active_terminal
            .get()
            .as_ref()
            .is_some_and(|active| {
                active.host_id == host_id_for_active && active.terminal_id == tid_for_active
            })
    };

    let tab_class = move || {
        if is_active() {
            "terminal-tab active"
        } else {
            "terminal-tab"
        }
    };

    let state_for_click = state.clone();
    let host_id_for_click = host_id.clone();
    let tid_for_click = terminal_id.clone();
    let on_click = move |_| {
        state_for_click
            .active_terminal
            .set(Some(crate::state::ActiveTerminalRef {
                host_id: host_id_for_click.clone(),
                terminal_id: tid_for_click.clone(),
            }));
    };

    let state_for_close = state.clone();
    let host_id_for_close = host_id;
    let tid_for_close = terminal_id;
    let on_close = move |ev: leptos::ev::MouseEvent| {
        ev.stop_propagation();
        let host_id = host_id_for_close.clone();
        let tid = tid_for_close.clone();
        let term = state_for_close
            .terminals
            .get_untracked()
            .into_iter()
            .find(|t| t.host_id == host_id && t.terminal_id == tid);
        let Some(term) = term else { return };
        if term.exited {
            remove_terminal(&state_for_close, &host_id, &tid);
            return;
        }
        let stream = term.stream.clone();
        let host_id_send = host_id.clone();
        spawn_local(async move {
            if let Err(e) = send_frame(
                &host_id_send,
                stream,
                FrameKind::TerminalClose,
                &TerminalClosePayload::default(),
            )
            .await
            {
                log::error!("failed to send terminal_close: {e}");
            }
        });
    };

    view! {
        <div class=tab_class>
            <button class="terminal-tab-button" on:click=on_click>
                <span class="terminal-tab-label">{label}</span>
                {move || exited().then(|| view! { <span class="terminal-tab-exited">"(exited)"</span> })}
            </button>
            <button class="terminal-tab-close" on:click=on_close title="Close terminal">
                "×"
            </button>
        </div>
    }
}

#[component]
fn TerminalContent(term: TerminalInfo) -> impl IntoView {
    let state = expect_context::<AppState>();
    let stream = term.stream.clone();
    let tid = term.terminal_id.clone();
    let host_id = term.host_id.clone();

    // Derive presentation fields reactively so metadata updates from
    // terminal_start / terminal_exit flow through without remounting.
    let state_for_term = state.clone();
    let host_id_for_term = host_id.clone();
    let tid_for_term = tid.clone();
    let lookup = move || {
        state_for_term
            .terminals
            .get()
            .into_iter()
            .find(|t| t.host_id == host_id_for_term && t.terminal_id == tid_for_term)
    };

    let lookup_for_status = lookup.clone();
    let status_text = move || match lookup_for_status() {
        Some(t) if t.exited => match t.exit_code {
            Some(code) => format!("Exited (code {code})"),
            None => t
                .exit_signal
                .clone()
                .map(|s| format!("Exited ({s})"))
                .unwrap_or_else(|| "Exited".to_string()),
        },
        Some(_) => "Running".to_string(),
        None => "Gone".to_string(),
    };

    let lookup_for_class = lookup.clone();
    let status_class = move || match lookup_for_class() {
        Some(t) if t.exited => "terminal-status exited",
        _ => "terminal-status running",
    };

    let lookup_for_info = lookup.clone();
    let info_text = move || match lookup_for_info() {
        Some(t) if !t.cwd.is_empty() => {
            if let Some(root) = &t.root {
                format!("{} · {} - {}", root_display_name(root), t.shell, t.cwd)
            } else {
                format!("{} - {}", t.shell, t.cwd)
            }
        }
        _ => "Starting...".to_string(),
    };

    let container_ref: NodeRef<leptos::html::Div> = NodeRef::new();
    let mount_host = host_id.clone();
    let mount_stream = stream.clone();
    let mount_tid = tid.clone();
    let mount_state = state.clone();

    Effect::new(move |_| {
        let Some(el) = container_ref.get() else {
            return;
        };
        let host_el: web_sys::HtmlElement = (*el).clone();
        let id_string = mount_tid.0.clone();

        // Outgoing user keystrokes -> TerminalSend
        let send_host = mount_host.clone();
        let send_stream = mount_stream.clone();
        let on_data = Closure::<dyn Fn(String)>::new(move |data: String| {
            let host_id = send_host.clone();
            let stream = send_stream.clone();
            spawn_local(async move {
                let payload = TerminalSendPayload { data };
                if let Err(e) =
                    send_frame(&host_id, stream, FrameKind::TerminalSend, &payload).await
                {
                    log::error!("failed to send terminal data: {e}");
                }
            });
        });

        // PTY size changes -> TerminalResize
        let resize_host = mount_host.clone();
        let resize_stream = mount_stream.clone();
        let on_resize = Closure::<dyn Fn(f64, f64)>::new(move |cols: f64, rows: f64| {
            let cols = cols as u16;
            let rows = rows as u16;
            if cols < 2 || rows < 1 {
                return;
            }
            let host_id = resize_host.clone();
            let stream = resize_stream.clone();
            spawn_local(async move {
                let payload = TerminalResizePayload { cols, rows };
                if let Err(e) =
                    send_frame(&host_id, stream, FrameKind::TerminalResize, &payload).await
                {
                    log::error!("failed to send terminal_resize: {e}");
                }
            });
        });

        if !term_bridge::create(&id_string, &host_el, on_data, on_resize) {
            log::error!("xterm bridge unavailable — terminal will not render");
            return;
        }

        // Drain any output that arrived before mount, mark as mounted.
        let drain_state = mount_state.clone();
        let drain_tid = mount_tid.clone();
        let drain_host = mount_host.clone();
        let mut drained: Vec<String> = Vec::new();
        drain_state.terminals.update(|terminals| {
            if let Some(t) = terminals
                .iter_mut()
                .find(|t| t.host_id == drain_host && t.terminal_id == drain_tid)
            {
                drained.append(&mut t.pending_output);
                t.widget_mounted = true;
            }
        });
        for chunk in drained {
            term_bridge::write(&id_string, &chunk);
        }

        term_bridge::focus(&id_string);

        // Dispose of the emulator (and drop stored JS callbacks) when the
        // component unmounts. State bookkeeping is handled separately since
        // `AppState` contains non-Send signals.
        let owner_id = id_string.clone();
        on_cleanup(move || {
            term_bridge::dispose(&owner_id);
        });

        // Flip `widget_mounted` back to false on unmount so late-arriving
        // output is buffered again rather than dropped.
        let cleanup_state = mount_state.clone();
        let cleanup_host = mount_host.clone();
        let cleanup_tid = mount_tid.clone();
        on_cleanup(move || {
            cleanup_state.terminals.update(|terminals| {
                if let Some(t) = terminals
                    .iter_mut()
                    .find(|t| t.host_id == cleanup_host && t.terminal_id == cleanup_tid)
                {
                    t.widget_mounted = false;
                }
            });
        });
    });

    // Refocus + refit when this terminal becomes the active one.
    let state_for_focus = state.clone();
    let tid_for_focus = tid.clone();
    Effect::new(move |_| {
        let active = state_for_focus.active_terminal.get();
        if active
            .as_ref()
            .is_some_and(|active| active.terminal_id == tid_for_focus)
        {
            term_bridge::fit(&tid_for_focus.0);
            term_bridge::focus(&tid_for_focus.0);
        }
    });

    let state_for_visible = state.clone();
    let tid_for_visible = tid.clone();
    let host_for_visible = host_id.clone();
    let content_class = move || {
        let active = state_for_visible.active_terminal.get();
        if active
            .as_ref()
            .is_some_and(|a| a.host_id == host_for_visible && a.terminal_id == tid_for_visible)
        {
            "terminal-content active"
        } else {
            "terminal-content"
        }
    };

    view! {
        <div class=content_class>
            <div class="terminal-info-bar">
                <span class="terminal-info-text">{info_text}</span>
                <span class=status_class>{status_text}</span>
            </div>
            <div class="terminal-xterm" node_ref=container_ref></div>
        </div>
    }
}

fn remove_terminal(state: &AppState, host_id: &str, tid: &TerminalId) {
    term_bridge::dispose(&tid.0);
    let tid_cloned = tid.clone();
    state.terminals.update(|terminals| {
        terminals.retain(|t| !(t.host_id == host_id && t.terminal_id == tid_cloned));
    });
    let active = state.active_terminal.get_untracked();
    if active
        .as_ref()
        .is_some_and(|a| a.host_id == host_id && &a.terminal_id == tid)
    {
        let next =
            state
                .terminals
                .get_untracked()
                .first()
                .map(|t| crate::state::ActiveTerminalRef {
                    host_id: t.host_id.clone(),
                    terminal_id: t.terminal_id.clone(),
                });
        state.active_terminal.set(next);
    }
}

fn short_id(id: &TerminalId) -> &str {
    let s = &id.0;
    if s.len() > 8 { &s[..8] } else { s }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use leptos::mount::mount_to;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

    fn install_send_stub() {
        js_sys::eval(
            r#"
            (function() {
                window.__test_terminal_send_calls = [];
                window.__TAURI__ = window.__TAURI__ || {};
                window.__TAURI__.core = window.__TAURI__.core || {};
                window.__TAURI__.core.invoke = function(cmd, args) {
                    window.__test_terminal_send_calls.push([cmd, JSON.stringify(args || {})]);
                    return Promise.resolve();
                };
            })();
            "#,
        )
        .expect("install send stub");
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

    #[wasm_bindgen_test]
    async fn home_terminal_opens_in_host_default_cwd() {
        install_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.selected_host_id.set(Some("local".to_owned()));
            state.host_streams.update(|streams| {
                streams.insert("local".to_owned(), StreamPath("/host/local".to_owned()));
            });
            provide_context(state);
            view! { <TerminalView /> }
        });

        next_tick().await;
        let button = container
            .query_selector(".terminal-new-btn")
            .unwrap()
            .expect("new terminal button");
        assert!(
            !button.has_attribute("disabled"),
            "Home must allow a terminal when its selected host is connected"
        );
        button
            .dyn_into::<HtmlElement>()
            .expect("terminal button is an HtmlElement")
            .click();
        for _ in 0..5 {
            next_tick().await;
        }

        let target = js_sys::eval(
            r#"
            (function() {
                for (const [cmd, args] of (window.__test_terminal_send_calls || [])) {
                    if (cmd !== "send_host_line") continue;
                    const envelope = JSON.parse(JSON.parse(args).line);
                    if (envelope.kind === "terminal_create") {
                        return JSON.stringify(envelope.payload.target);
                    }
                }
                return "";
            })()
            "#,
        )
        .expect("probe terminal create")
        .as_string()
        .unwrap_or_default();
        assert_eq!(
            target, r#"{"kind":"host_default"}"#,
            "Home terminal must ask the host to launch in Tyde's cwd"
        );
    }
}
