use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::send::send_frame;
use crate::state::{AppState, DockVisibility, TerminalInfo};

use protocol::{
    FrameKind, TerminalCreatePayload, TerminalId, TerminalLaunchTarget, TerminalSendPayload,
};

#[component]
pub fn TerminalView() -> impl IntoView {
    let state = expect_context::<AppState>();

    let terminals = move || state.terminals.get();
    let active_id = move || state.active_terminal_id.get();

    let active_terminal = move || {
        let id = active_id()?;
        let terms = terminals();
        terms.into_iter().find(|t| t.terminal_id == id)
    };

    view! {
        <div class="terminal-view">
            <TerminalTabBar />
            <div class="terminal-body">
                {move || match active_terminal() {
                    Some(term) => view! { <TerminalContent term=term /> }.into_any(),
                    None => view! {
                        <div class="terminal-empty">
                            <span class="terminal-empty-text">"No terminal open"</span>
                        </div>
                    }.into_any(),
                }}
            </div>
        </div>
    }
}

#[component]
fn TerminalTabBar() -> impl IntoView {
    let state = expect_context::<AppState>();

    let can_create_terminal = move || {
        let pid = state.active_project_id.get()?;
        let projects = state.projects.get();
        let proj = projects.iter().find(|p| p.id == pid)?;
        let root = proj.roots.first().cloned()?;
        Some((pid, root))
    };

    let on_new_terminal = move |_| {
        let host_id = match state.host_id.get() {
            Some(id) => id,
            None => return,
        };
        let host_stream = match state.host_stream.get() {
            Some(s) => s,
            None => return,
        };

        let (pid, root) = match can_create_terminal() {
            Some(v) => v,
            None => return,
        };

        let target = TerminalLaunchTarget::Project {
            project_id: pid,
            root: protocol::ProjectRootPath(root),
            relative_cwd: None,
        };

        let payload = TerminalCreatePayload {
            target,
            cols: 80,
            rows: 24,
        };

        // Show bottom dock
        state.bottom_dock.set(DockVisibility::Visible);

        spawn_local(async move {
            if let Err(e) =
                send_frame(&host_id, host_stream, FrameKind::TerminalCreate, &payload).await
            {
                log::error!("failed to create terminal: {e}");
            }
        });
    };

    let btn_disabled = move || can_create_terminal().is_none();

    view! {
        <div class="terminal-tab-bar">
            <div class="terminal-tabs">
                <For
                    each=move || state.terminals.get()
                    key=|t| t.terminal_id.clone()
                    let:term
                >
                    <TerminalTab term=term />
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
fn TerminalTab(term: TerminalInfo) -> impl IntoView {
    let state = expect_context::<AppState>();
    let tid = term.terminal_id.clone();
    let tid_for_click = tid.clone();

    let label = if term.shell.is_empty() {
        format!("Terminal {}", short_id(&tid))
    } else {
        term.shell.clone()
    };

    let is_active = move || {
        state
            .active_terminal_id
            .get()
            .as_ref()
            .is_some_and(|id| *id == tid)
    };

    let tab_class = move || {
        if is_active() {
            "terminal-tab active"
        } else {
            "terminal-tab"
        }
    };

    let on_click = move |_| {
        state.active_terminal_id.set(Some(tid_for_click.clone()));
    };

    let exited = term.exited;

    view! {
        <button class=tab_class on:click=on_click>
            <span class="terminal-tab-label">{label}</span>
            {if exited {
                Some(view! { <span class="terminal-tab-exited">"(exited)"</span> })
            } else {
                None
            }}
        </button>
    }
}

#[component]
fn TerminalContent(term: TerminalInfo) -> impl IntoView {
    let state = expect_context::<AppState>();
    let terminal_input = RwSignal::new(String::new());
    let stream = term.stream.clone();
    let tid = term.terminal_id.clone();

    let status_text = if term.exited {
        match term.exit_code {
            Some(code) => format!("Exited (code {code})"),
            None => "Exited".to_string(),
        }
    } else {
        "Running".to_string()
    };

    let status_class = if term.exited {
        "terminal-status exited"
    } else {
        "terminal-status running"
    };

    let info_text = if term.cwd.is_empty() {
        "Starting...".to_string()
    } else {
        format!("{} - {}", term.shell, term.cwd)
    };

    let tid_for_output = tid.clone();
    let output = move || {
        let terms = state.terminals.get();
        terms
            .iter()
            .find(|t| t.terminal_id == tid_for_output)
            .map(|t| t.output_buffer.clone())
            .unwrap_or_default()
    };

    let is_exited = move || {
        let terms = state.terminals.get();
        terms
            .iter()
            .find(|t| t.terminal_id == tid)
            .is_none_or(|t| t.exited)
    };

    let stored_stream = StoredValue::new(stream);

    let send_data = move |data: String| {
        let host_id = match state.host_id.get() {
            Some(id) => id,
            None => return,
        };
        let stream = stored_stream.get_value();
        spawn_local(async move {
            let payload = TerminalSendPayload { data };
            if let Err(e) = send_frame(&host_id, stream, FrameKind::TerminalSend, &payload).await {
                log::error!("failed to send terminal data: {e}");
            }
        });
    };

    let on_keydown = move |ev: leptos::ev::KeyboardEvent| {
        if ev.key() == "Enter" {
            ev.prevent_default();
            let text = terminal_input.get();
            if !text.is_empty() {
                terminal_input.set(String::new());
                send_data(format!("{text}\n"));
            }
        } else if ev.key() == "c" && ev.ctrl_key() {
            ev.prevent_default();
            send_data("\x03".to_string());
        }
    };

    let on_click_send = move |_| {
        let text = terminal_input.get();
        if !text.is_empty() {
            terminal_input.set(String::new());
            send_data(format!("{text}\n"));
        }
    };

    view! {
        <div class="terminal-content">
            <div class="terminal-info-bar">
                <span class="terminal-info-text">{info_text}</span>
                <span class=status_class>{status_text}</span>
            </div>
            <pre class="terminal-output">{output}</pre>
            <Show when=move || !is_exited()>
                <div class="terminal-input-area">
                    <span class="terminal-prompt">"$ "</span>
                    <input
                        class="terminal-input"
                        type="text"
                        placeholder="Type a command..."
                        prop:value=move || terminal_input.get()
                        on:input=move |ev| terminal_input.set(event_target_value(&ev))
                        on:keydown=on_keydown
                    />
                    <button
                        class="terminal-send-btn"
                        disabled=move || terminal_input.get().is_empty()
                        on:click=on_click_send
                    >
                        <svg width="14" height="14" viewBox="0 0 16 16" fill="none">
                            <path d="M2 8L14 8M14 8L9 3M14 8L9 13" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>
                        </svg>
                    </button>
                </div>
            </Show>
        </div>
    }
}

fn short_id(id: &TerminalId) -> &str {
    let s = &id.0;
    if s.len() > 8 { &s[..8] } else { s }
}
