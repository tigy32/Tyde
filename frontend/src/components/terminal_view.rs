use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::spawn_local;

use crate::send::send_frame;
use crate::state::{AppState, DockVisibility, TerminalInfo};
use crate::term_bridge;

use protocol::{
    FrameKind, TerminalClosePayload, TerminalCreatePayload, TerminalId, TerminalLaunchTarget,
    TerminalResizePayload, TerminalSendPayload,
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

    let can_create_terminal = move || {
        let active_project = state_for_can_create.active_project.get()?;
        let project = state_for_can_create.active_project_info_untracked()?;
        let root = project.project.roots.first().cloned()?;
        Some((active_project, root))
    };

    let state_for_new_terminal = state.clone();
    let can_create_terminal_for_new = can_create_terminal.clone();
    let on_new_terminal = move |_| {
        let (active_project, root) = match can_create_terminal_for_new() {
            Some(v) => v,
            None => return,
        };
        let host_id = active_project.host_id.clone();
        let host_stream = match state_for_new_terminal.host_stream_untracked(&host_id) {
            Some(stream) => stream,
            None => return,
        };

        let target = TerminalLaunchTarget::Project {
            project_id: active_project.project_id,
            root: protocol::ProjectRootPath(root),
            relative_cwd: None,
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

    let btn_disabled = move || can_create_terminal().is_none();
    let state_for_tabs = state.clone();

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
        Some(t) if !t.shell.is_empty() => t.shell,
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
        Some(t) if !t.cwd.is_empty() => format!("{} - {}", t.shell, t.cwd),
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
