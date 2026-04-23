use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::actions::begin_new_chat;
use crate::send::send_frame;
use crate::state::{AppState, DockVisibility, TabContent};

use protocol::{FrameKind, ProjectFileKind, ProjectRefreshPayload, StreamPath};

#[derive(Clone, Debug)]
struct CommandEntry {
    name: &'static str,
    shortcut: Option<&'static str>,
    id: &'static str,
}

const COMMANDS: &[CommandEntry] = &[
    CommandEntry {
        name: "New Chat",
        shortcut: Some("Ctrl+N"),
        id: "new_chat",
    },
    CommandEntry {
        name: "Toggle Left Panel",
        shortcut: None,
        id: "toggle_left",
    },
    CommandEntry {
        name: "Toggle Right Panel",
        shortcut: None,
        id: "toggle_right",
    },
    CommandEntry {
        name: "Toggle Bottom Panel",
        shortcut: None,
        id: "toggle_bottom",
    },
    CommandEntry {
        name: "Go to Home",
        shortcut: None,
        id: "go_home",
    },
    CommandEntry {
        name: "Go to Chat",
        shortcut: None,
        id: "go_chat",
    },
    CommandEntry {
        name: "Open Settings",
        shortcut: Some("Ctrl+,"),
        id: "open_settings",
    },
    CommandEntry {
        name: "Refresh Project",
        shortcut: None,
        id: "refresh_project",
    },
    CommandEntry {
        name: "Send Feedback",
        shortcut: None,
        id: "send_feedback",
    },
];

#[derive(Clone, Debug, PartialEq)]
enum PaletteResult {
    File { name: String, path: String },
    Command { entry_index: usize },
}

fn fuzzy_score(query: &str, target: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }
    let query_lower = query.to_lowercase();
    let target_lower = target.to_lowercase();

    if target_lower.starts_with(&query_lower) {
        return Some(100);
    }

    let words: Vec<&str> = target_lower.split(['/', '.', '_', '-', ' ']).collect();
    for word in &words {
        if word.starts_with(&query_lower) {
            return Some(75);
        }
    }

    if target_lower.contains(&query_lower) {
        return Some(50);
    }

    None
}

fn toggle_dock(signal: RwSignal<DockVisibility>) {
    signal.update(|v: &mut DockVisibility| {
        *v = match v {
            DockVisibility::Visible => DockVisibility::Hidden,
            DockVisibility::Hidden => DockVisibility::Visible,
        };
    });
}

fn execute_command(state: &AppState, id: &str) {
    match id {
        "new_chat" => {
            begin_new_chat(state, None);
        }
        "toggle_left" => toggle_dock(state.left_dock),
        "toggle_right" => toggle_dock(state.right_dock),
        "toggle_bottom" => toggle_dock(state.bottom_dock),
        "go_home" => state.open_tab(TabContent::Home, "Home".to_string(), false),
        "go_chat" => {
            // Activate the most recent chat tab, or open a new chat
            let found = state.center_zone.with_untracked(|cz| {
                cz.tabs
                    .iter()
                    .rev()
                    .find(|t| matches!(t.content, TabContent::Chat { .. }))
                    .map(|t| t.id)
            });
            if let Some(id) = found {
                state.activate_tab(id);
            } else {
                begin_new_chat(state, None);
            }
        }
        "open_settings" => {
            state.command_palette_open.set(false);
            state.settings_open.set(true);
        }
        "send_feedback" => {
            state.command_palette_open.set(false);
            state.feedback_open.set(true);
        }
        "refresh_project" => {
            let active_project = state.active_project_ref_untracked();
            if let Some(active_project) = active_project {
                spawn_local(async move {
                    let stream = StreamPath(format!("/project/{}", active_project.project_id.0));
                    if let Err(e) = send_frame(
                        &active_project.host_id,
                        stream,
                        FrameKind::ProjectRefresh,
                        &ProjectRefreshPayload {},
                    )
                    .await
                    {
                        log::error!("failed to send ProjectRefresh: {e}");
                    }
                });
            }
        }
        _ => log::warn!("unknown command: {id}"),
    }
}

/// Perform the select action for a given result index.
/// Uses expect_context to avoid capturing the non-Copy AppState.
fn do_select(results: Memo<Vec<PaletteResult>>, idx: usize) {
    let state = expect_context::<AppState>();
    let items = results.get();
    if idx >= items.len() {
        return;
    }
    match &items[idx] {
        PaletteResult::File { path, .. } => {
            crate::actions::open_file(&state, path);
        }
        PaletteResult::Command { entry_index } => {
            execute_command(&state, COMMANDS[*entry_index].id);
        }
    }
    state.command_palette_open.set(false);
}

#[component]
pub fn CommandPalette() -> impl IntoView {
    let state = expect_context::<AppState>();
    let open = state.command_palette_open;
    let file_tree = state.file_tree;

    let input = RwSignal::new(String::new());
    let selected_index = RwSignal::new(0usize);

    let is_command_mode = Memo::new(move |_| input.get().starts_with('>'));

    let results: Memo<Vec<PaletteResult>> = Memo::new(move |_| {
        let query_raw = input.get();
        let command_mode = query_raw.starts_with('>');

        if command_mode {
            let query = query_raw[1..].trim();
            let mut scored: Vec<(usize, u32)> = COMMANDS
                .iter()
                .enumerate()
                .filter_map(|(i, cmd)| {
                    if query.is_empty() {
                        Some((i, 0))
                    } else {
                        fuzzy_score(query, cmd.name).map(|s| (i, s))
                    }
                })
                .collect();
            scored.sort_by(|a, b| b.1.cmp(&a.1));
            scored
                .into_iter()
                .take(10)
                .map(|(i, _)| PaletteResult::Command { entry_index: i })
                .collect()
        } else {
            let query = query_raw.trim();
            let tree = file_tree.get();
            let mut scored: Vec<(String, String, u32)> = Vec::new();
            for (_pid, entries) in tree.iter() {
                for entry in entries {
                    if entry.kind != ProjectFileKind::File {
                        continue;
                    }
                    let path = &entry.relative_path;
                    let file_name = path.rsplit('/').next().unwrap_or(path);
                    let score = if query.is_empty() {
                        Some(0)
                    } else {
                        fuzzy_score(query, file_name).or_else(|| fuzzy_score(query, path))
                    };
                    if let Some(s) = score {
                        scored.push((file_name.to_owned(), path.clone(), s));
                    }
                }
            }
            scored.sort_by(|a, b| b.2.cmp(&a.2));
            scored
                .into_iter()
                .take(10)
                .map(|(name, path, _)| PaletteResult::File { name, path })
                .collect()
        }
    });

    let result_count = Memo::new(move |_| results.get().len());

    let on_keydown = move |ev: web_sys::KeyboardEvent| match ev.key().as_str() {
        "Escape" => {
            ev.prevent_default();
            open.set(false);
        }
        "ArrowDown" => {
            ev.prevent_default();
            let count = result_count.get();
            if count > 0 {
                selected_index.update(|i: &mut usize| *i = (*i + 1) % count);
            }
        }
        "ArrowUp" => {
            ev.prevent_default();
            let count = result_count.get();
            if count > 0 {
                selected_index.update(|i: &mut usize| {
                    *i = if *i == 0 { count - 1 } else { *i - 1 };
                });
            }
        }
        "Enter" => {
            ev.prevent_default();
            do_select(results, selected_index.get_untracked());
        }
        _ => {}
    };

    let on_input = move |ev: web_sys::Event| {
        let target = ev.target().unwrap();
        let el: web_sys::HtmlInputElement = target.unchecked_into();
        input.set(el.value());
        selected_index.set(0);
    };

    let on_backdrop_click = move |_| {
        open.set(false);
    };

    let input_ref = NodeRef::<leptos::html::Input>::new();

    Effect::new(move |_| {
        if open.get() {
            input.set(String::new());
            selected_index.set(0);
            if let Some(el) = input_ref.get() {
                let _ = el.focus();
            }
        }
    });

    let mode_label = move || {
        if is_command_mode.get() {
            "Commands"
        } else {
            "Files"
        }
    };

    view! {
        <Show when=move || open.get()>
            <div class="cp-overlay" on:click=on_backdrop_click>
                <div class="cp-modal" on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()>
                    <div class="cp-header">
                        <input
                            node_ref=input_ref
                            class="cp-input"
                            type="text"
                            placeholder="Search files... (type > for commands)"
                            on:input=on_input
                            on:keydown=on_keydown
                            prop:value=move || input.get()
                            spellcheck="false"
                            {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                            autocapitalize="none"
                            autocomplete="off"
                        />
                        <span class="cp-mode-badge">{mode_label}</span>
                    </div>
                    <div class="cp-results">
                        {move || {
                            results.get().into_iter().enumerate().map(|(idx, result)| {
                                let is_selected = move || selected_index.get() == idx;
                                let on_click = move |_| {
                                    selected_index.set(idx);
                                    do_select(results, idx);
                                };
                                match result {
                                    PaletteResult::File { name, path } => {
                                        view! {
                                            <div
                                                class="cp-result-item"
                                                class:selected=is_selected
                                                on:click=on_click
                                            >
                                                <span class="cp-file-name">{name}</span>
                                                <span class="cp-file-path">{path}</span>
                                            </div>
                                        }.into_any()
                                    }
                                    PaletteResult::Command { entry_index } => {
                                        let cmd = &COMMANDS[entry_index];
                                        let name = cmd.name;
                                        let shortcut = cmd.shortcut;
                                        view! {
                                            <div
                                                class="cp-result-item"
                                                class:selected=is_selected
                                                on:click=on_click
                                            >
                                                <span class="cp-cmd-name">{name}</span>
                                                {shortcut.map(|s| view! {
                                                    <kbd class="cp-cmd-shortcut">{s}</kbd>
                                                })}
                                            </div>
                                        }.into_any()
                                    }
                                }
                            }).collect_view()
                        }}
                        <Show when=move || results.get().is_empty()>
                            <div class="cp-empty">"No results"</div>
                        </Show>
                    </div>
                </div>
            </div>
        </Show>
    }
}
