use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::send::send_frame;
use crate::state::{AppState, CenterView};

use protocol::{FrameKind, ProjectCreatePayload};

#[component]
pub fn ProjectRail() -> impl IntoView {
    let state = expect_context::<AppState>();

    let projects = move || state.projects.get();
    let active_id = move || state.active_project_id.get();

    let go_home = move |_| {
        state.active_project_id.set(None);
        state.center_view.set(CenterView::Home);
    };

    let home_class = move || {
        if active_id().is_none() && state.center_view.get() == CenterView::Home {
            "rail-item rail-home active"
        } else {
            "rail-item rail-home"
        }
    };

    let connected = Memo::new(move |_| state.host_id.get().is_some());

    let adding = state.adding_project;

    let add_project = move |_| {
        adding.set(true);
    };

    let on_add_submit = move |path: String| {
        adding.set(false);
        let path = path.trim().to_owned();
        if path.is_empty() {
            return;
        }
        let name = path
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or(&path)
            .to_owned();
        let host_id = state.host_id.get_untracked();
        let host_stream = state.host_stream.get_untracked();
        if let (Some(hid), Some(hs)) = (host_id, host_stream) {
            spawn_local(async move {
                if let Err(e) = send_frame(
                    &hid,
                    hs,
                    FrameKind::ProjectCreate,
                    &ProjectCreatePayload {
                        name,
                        roots: vec![path],
                    },
                )
                .await
                {
                    log::error!("failed to send ProjectCreate: {e}");
                }
            });
        }
    };

    let on_add_cancel = move || {
        adding.set(false);
    };

    view! {
        <nav class="project-rail">
            <div class="rail-items">
                <button class=home_class on:click=go_home title="Home">
                    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                        <path d="M3 9l9-7 9 7v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>
                        <polyline points="9 22 9 12 15 12 15 22"/>
                    </svg>
                </button>

                <div class="rail-divider"></div>

                <For
                    each=projects
                    key=|p| p.id.0.clone()
                    let:project
                >
                    {
                        let pid = project.id.clone();
                        let name = project.name.clone();
                        let initial = project.name.chars().next().unwrap_or('?').to_uppercase().to_string();
                        let hue = name_to_hue(&name);
                        let style = format!("background: hsl({hue}, 45%, 35%); color: hsl({hue}, 60%, 85%)");

                        let state = state.clone();
                        let pid_click = pid.clone();
                        let on_click = move |_| {
                            state.active_project_id.set(Some(pid_click.clone()));
                        };

                        let is_active = {
                            let pid = pid.clone();
                            move || active_id().as_ref().map(|id| *id == pid).unwrap_or(false)
                        };

                        let item_class = move || {
                            if is_active() {
                                "rail-item rail-project active"
                            } else {
                                "rail-item rail-project"
                            }
                        };

                        view! {
                            <button class=item_class style=style title=name on:click=on_click>
                                {initial}
                            </button>
                        }
                    }
                </For>
            </div>

            <div class="rail-bottom">
                <Show when=move || adding.get()>
                    <AddProjectInput on_submit=on_add_submit on_cancel=on_add_cancel />
                </Show>
                <button class="rail-item rail-add" title="New Project" on:click=add_project disabled=move || !connected.get()>
                    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                        <line x1="12" y1="5" x2="12" y2="19"/>
                        <line x1="5" y1="12" x2="19" y2="12"/>
                    </svg>
                </button>
            </div>
        </nav>
    }
}

#[component]
fn AddProjectInput(
    on_submit: impl Fn(String) + 'static + Copy,
    on_cancel: impl Fn() + 'static + Copy,
) -> impl IntoView {
    let input_ref = NodeRef::<leptos::html::Input>::new();

    Effect::new(move |_| {
        if let Some(el) = input_ref.get() {
            let _ = el.focus();
        }
    });

    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        match ev.key().as_str() {
            "Enter" => {
                if let Some(el) = input_ref.get_untracked() {
                    on_submit(el.value());
                }
            }
            "Escape" => {
                on_cancel();
            }
            _ => {}
        }
    };

    view! {
        <div class="add-project-popover">
            <input
                node_ref=input_ref
                class="add-project-input"
                type="text"
                placeholder="/path/to/workspace"
                on:keydown=on_keydown
            />
        </div>
    }
}

/// Derive a stable hue (0..360) from a project name for the avatar color.
fn name_to_hue(name: &str) -> u32 {
    let hash: u32 = name.bytes().fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    hash % 360
}
